//! The default exclusion policy, and the defects it must not reproduce.
//!
//! Three separate failures are covered here, and they are distinct enough to deserve separate
//! tests:
//!
//! 1. Over-broad defaults that drop real source (`build`, `target`, `dist` in Cortex's list).
//! 2. The root-directory trap: a checkout named `build` producing a silently empty index.
//! 3. Case incoherence: a case-sensitive skip list on a case-insensitive filesystem.

use std::collections::BTreeSet;

use super::discover_fixture;
use super::support::{TempTree, contains, paths};
use crate::discover::{DiscoveryOptions, ExcludeSet, FileDiscovery};

/// The default set prunes build output and dependency trees that no ignore file mentions.
#[test]
fn default_set_prunes_build_output_and_dependencies() {
    let discovery = discover_fixture();

    assert!(
        !contains(discovery.files(), "target/debug/tool.rs"),
        "target/ is in the default set and no ignore file mentions it"
    );
    assert!(
        !contains(discovery.files(), "node_modules/dep/index.js"),
        "node_modules/ is in the default set and no ignore file mentions it"
    );
    assert!(
        !contains(discovery.files(), "__pycache__/cached.py"),
        "__pycache__/ is in the default set and no ignore file mentions it"
    );
}

/// A pruned directory is not merely filtered out of the result — the walk does not descend into
/// it, so its contents cost nothing. `stats.excluded` is the only honest measure of this, and it
/// is populated by the pruning hook rather than by a post-filter.
#[test]
fn excluded_directories_are_pruned_not_merely_filtered() {
    let discovery = discover_fixture();
    let stats = discovery.stats();

    assert!(
        stats.excluded >= 3,
        "at least the three excluded fixture directories must be counted, got {}",
        stats.excluded
    );
    assert!(
        stats.files_examined < stats.entries_visited,
        "pruning must reduce the files examined below the entries visited"
    );
}

/// The overridability requirement, in the form a real user hits it: a repository whose `build/`
/// holds hand-written code.
///
/// Cortex's list was hardcoded inside the walker, so this repository could not be indexed
/// correctly by any configuration at all. Here it takes one call.
#[test]
fn a_build_directory_can_be_overridden() {
    let root = TempTree::new("build-override");
    root.file("build/lexer.rs", "pub fn lex() {}");
    root.file("target/debug/out.rs", "pub fn artifact() {}");
    root.file("src/main.rs", "fn main() {}");

    let options = DiscoveryOptions::default()
        .with_excludes(ExcludeSet::default().without(["build", "target"]));
    let discovery = FileDiscovery::new(root.path(), options)
        .discover()
        .expect("discover");

    assert!(
        contains(discovery.files(), "build/lexer.rs"),
        "removing `build` from the set must make a source build directory visible"
    );
    assert!(
        contains(discovery.files(), "target/debug/out.rs"),
        "removing `target` must make it visible too"
    );
    assert!(contains(discovery.files(), "src/main.rs"));
}

/// The root-directory trap, in full.
///
/// Cortex applied its skip filter to the walk root. A repository checked out into a directory
/// literally named `build` therefore produced zero files, `file_count: 0`, **no error**, and a
/// daemon that read that as "never indexed" and rebuilt forever. The whole point of this test is
/// that the failure is now loud and impossible.
#[test]
fn a_checkout_named_build_still_finds_its_files() {
    let root = TempTree::new("build");
    let checkout = root.path().join("build");
    std::fs::create_dir_all(checkout.join("src")).expect("create checkout");

    // The checkout directory is named `build`, which is in the default exclusion set. Its contents
    // are ordinary source.
    std::fs::write(checkout.join("src/main.rs"), "fn main() {}").expect("write source");
    std::fs::write(checkout.join("src/lib.rs"), "pub fn f() {}").expect("write source");

    let discovery = FileDiscovery::new(&checkout, DiscoveryOptions::default())
        .discover()
        .expect("a checkout named build must be discoverable");

    let found = paths(discovery.files());
    assert_eq!(
        found,
        vec!["src/lib.rs", "src/main.rs"],
        "the root's own name must never be matched against the exclusion set"
    );
    assert!(!discovery.is_empty());
    assert_eq!(discovery.stats().files_yielded, 2);
}

/// The same trap, for every name in the default set, because the defence is structural and should
/// hold for all of them rather than for `build` specifically.
#[test]
fn no_name_in_the_default_set_can_exclude_a_checkout() {
    for name in ExcludeSet::default().names() {
        let root = TempTree::new("named-root");
        let checkout = root.path().join(name);
        std::fs::create_dir_all(&checkout).expect("create checkout");
        std::fs::write(checkout.join("main.rs"), "fn main() {}").expect("write source");

        let discovery = FileDiscovery::new(&checkout, DiscoveryOptions::default())
            .discover()
            .expect("discover");

        assert_eq!(
            discovery.stats().files_yielded,
            1,
            "a checkout named `{name}` must still yield its source"
        );
    }
}

/// `ExcludeSet::excludes` only ever sees relative paths, which is the structural reason the root
/// trap cannot recur. Asserted directly, because the fix is a property of this function and not of
/// the walker's ordering.
#[test]
fn exclusion_matching_only_sees_relative_paths() {
    let set = ExcludeSet::default();

    assert!(set.excludes(std::path::Path::new("target/debug")));
    assert!(set.excludes(std::path::Path::new("crates/x/target/debug")));
    // The root's own name is not in any relative path, so no relative path can match it by
    // construction. This is the assertion the root trap would fail if the code were changed to
    // match against absolute paths.
    assert!(
        !set.excludes(std::path::Path::new("crates/x/src/main.rs")),
        "an ordinary path must not match any default name"
    );
}

/// Case incoherence: the skip list was case-sensitive while the store key was lowercased, so on
/// NTFS `Target/` and `NODE_MODULES/` were indexed and then folded. The set must match
/// case-insensitively so that a differently-cased directory is excluded on every platform.
#[test]
fn exclusion_matching_is_case_insensitive() {
    let set = ExcludeSet::default();
    let variants = ["TARGET", "Target", "target", "tArGeT"];

    for variant in variants {
        assert!(
            set.excludes(std::path::Path::new(variant)),
            "`{variant}` must be excluded on a case-insensitive filesystem"
        );
        assert!(
            set.excludes(&std::path::Path::new(variant).join("inner.rs")),
            "`{variant}/inner.rs` must be excluded"
        );
    }

    for variant in ["NODE_MODULES", "Node_Modules", "node_modules"] {
        assert!(
            set.excludes(std::path::Path::new(variant)),
            "`{variant}` must be excluded"
        );
    }
}

/// Names are stored lowercased, so `names()` reports the canonical spelling and a caller reading the
/// printed policy sees exactly what is matched.
#[test]
fn names_are_stored_lowercased() {
    let custom = ExcludeSet::default().with(["Custom_Dir"]);
    let names: BTreeSet<&str> = custom.names().collect();

    assert!(
        names.contains("custom_dir"),
        "names must be lowercased: {names:?}"
    );
    assert!(
        !names.contains("Custom_Dir"),
        "the original spelling must not survive: {names:?}"
    );
}

/// Hidden files are indexed by default. Cortex inherited the `ignore` crate's default of skipping
/// them, so its index contained no CI configuration — which is a large fraction of what an agent
/// needs to know about a repository.
#[test]
fn hidden_paths_are_indexed_by_default() {
    let discovery = discover_fixture();

    assert!(
        contains(discovery.files(), ".github/scripts/setup.rs"),
        "a hidden directory holding indexable source must be walked by default"
    );
}

/// ...and the option to skip them works, so the default is a decision rather than an oversight.
#[test]
fn hidden_paths_can_be_skipped() {
    let options = DiscoveryOptions::default().with_hidden_skipped(true);
    let discovery = super::discover_fixture_with(options);

    assert!(
        !contains(discovery.files(), ".github/scripts/setup.rs"),
        "with hidden paths skipped, a dot-prefixed directory must not be walked"
    );
    assert!(
        contains(discovery.files(), "src/main.rs"),
        "skipping hidden paths must not affect ordinary source"
    );
}

#[test]
fn an_empty_exclusion_set_walks_everything_the_walker_offers() {
    let options = DiscoveryOptions::default().with_excludes(ExcludeSet::empty());
    let discovery = super::discover_fixture_with(options);

    assert!(
        contains(discovery.files(), "target/debug/tool.rs"),
        "an empty set must not prune build output"
    );
    assert!(
        !contains(discovery.files(), "toponly.rs"),
        "an empty set must not disable .gitignore; the two are independent"
    );
    assert_eq!(discovery.stats().excluded, 0);
}
