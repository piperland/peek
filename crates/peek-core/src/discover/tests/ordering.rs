//! Deterministic ordering and case preservation.
//!
//! Two runs over an unchanged tree must produce identical output. Incremental update diffs two
//! file lists, and every test that compares two discoveries relies on it, so a non-deterministic
//! order would not be a cosmetic problem: it would make the engine's own state churn on a
//! filesystem that changed nothing.

use super::support::{TempTree, contains};
use crate::discover::{CaseSensitivity, DiscoveryOptions, FileDiscovery};
use crate::model::RepoPath;

/// The same tree, walked twice, gives byte-identical results.
#[test]
fn two_walks_of_an_unchanged_tree_agree() {
    let root = TempTree::new("determinism");
    for index in 0..40 {
        root.file(&format!("src/mod{index:02}.rs"), "fn f() {}");
        root.file(&format!("src/nested/deep{index:02}.py"), "VALUE = 1");
    }

    let options = DiscoveryOptions::default();
    let first = FileDiscovery::new(root.path(), options.clone())
        .discover()
        .expect("first walk")
        .into_report();
    let second = FileDiscovery::new(root.path(), options)
        .discover()
        .expect("second walk")
        .into_report();

    assert_eq!(
        first.files(),
        second.files(),
        "two walks must agree exactly"
    );
    assert_eq!(first.stats(), second.stats());
}

/// Results are sorted by path, so a consumer can binary-search and a diff can be positional.
#[test]
fn results_are_sorted_by_path() {
    let root = TempTree::new("sorted");
    for name in ["zebra.rs", "alpha.rs", "Middle.rs", "beta.rs"] {
        root.file(&format!("src/{name}"), "fn f() {}");
    }
    for name in ["c.rs", "a.rs", "b.rs"] {
        root.file(&format!("src/sub/{name}"), "fn f() {}");
    }

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    let found: Vec<&str> = discovery
        .files()
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let mut sorted = found.clone();
    sorted.sort_unstable();

    assert_eq!(
        found, sorted,
        "output must already be in path order: {found:?}"
    );
    // Byte order, not a locale collation, so the order is the same on every machine.
    assert_eq!(found[0], "src/Middle.rs");
    assert_eq!(found[found.len() - 1], "src/zebra.rs");
}

/// Case is preserved end to end. Cortex lowercased paths before using them as store keys, so on a
/// case-sensitive filesystem `src/Foo.rs` and `src/foo.rs` collapsed into one entry and one
/// silently overwrote the other.
#[test]
fn output_paths_preserve_case() {
    let root = TempTree::new("case-preserved");
    if !root.filesystem_is_case_sensitive() {
        eprintln!(
            "skipping: this filesystem is case-insensitive, so Foo.rs and foo.rs cannot both exist"
        );
        return;
    }
    root.file("src/Foo.rs", "fn a() {}");
    root.file("src/foo.rs", "fn b() {}");
    root.file("src/MixedCase/DeepFile.py", "VALUE = 1");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert!(
        contains(discovery.files(), "src/Foo.rs"),
        "an upper-case name must survive the walk unchanged"
    );
    assert!(
        contains(discovery.files(), "src/foo.rs"),
        "a lower-case name must survive too"
    );
    assert!(contains(discovery.files(), "src/MixedCase/DeepFile.py"));

    for file in discovery.files() {
        assert_eq!(
            file.path,
            RepoPath::new(file.path.as_str()).expect("round trips"),
            "a path that does not round-trip through RepoPath has been mangled"
        );
    }
    assert!(
        discovery.files().iter().any(|file| file
            .path
            .as_str()
            .chars()
            .any(|c| c.is_ascii_uppercase())),
        "at least one yielded path must contain an upper-case letter, or this test proves nothing"
    );
}

/// Sorting is case-preserving rather than case-folded. ASCII order puts upper case first, so
/// `Foo.rs` precedes `foo.rs` — a total order that does not require the files to be distinct.
#[test]
fn sorting_does_not_fold_case() {
    let root = TempTree::new("sort-case");
    if !root.filesystem_is_case_sensitive() {
        eprintln!(
            "skipping: this filesystem is case-insensitive, so a case fold cannot be observed"
        );
        return;
    }
    root.file("src/b.rs", "fn b() {}");
    root.file("src/A.rs", "fn a() {}");
    root.file("src/a.rs", "fn a() {}");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    let found: Vec<&str> = discovery
        .files()
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(found, vec!["src/A.rs", "src/a.rs", "src/b.rs"]);
}

/// Two paths differing only in case are duplicates exactly when the volume says they are.
///
/// The volume is the CI filesystem, and the assertion branches on what was detected rather than on
/// the platform, because the detection itself is a separate claim. On a case-sensitive volume the
/// two files coexist and are both yielded; on a case-insensitive one the second write overwrote the
/// first, so there is only one file and nothing to deduplicate.
#[test]
fn duplicate_detection_follows_the_filesystem() {
    let root = TempTree::new("duplicates");
    root.file("src/Thing.rs", "fn a() {}");
    let second_written = std::fs::write(root.path().join("src/thing.rs"), "fn b() {}").is_ok();
    let canonical = std::fs::canonicalize(root.path()).expect("canonicalize");
    let detected = CaseSensitivity::detect(&canonical);
    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert!(contains(discovery.files(), "src/Thing.rs"));
    if detected.is_insensitive() {
        assert!(
            !second_written || discovery.stats().files_yielded == 1,
            "a case-insensitive volume holds one of the two spellings, not both"
        );
    } else {
        assert!(
            second_written,
            "a case-sensitive volume must accept both spellings"
        );
        assert!(contains(discovery.files(), "src/thing.rs"));
        assert_eq!(
            discovery.stats().duplicates,
            0,
            "two distinct files are not duplicates on a case-sensitive volume"
        );
    }
}

/// Forcing case-insensitive duplicate detection makes the duplicate branch observable, and
/// therefore testable on a case-sensitive volume.
///
/// On a case-insensitive volume the two spellings cannot coexist, so the *conclusion* — one file
/// yielded — still holds and is still asserted; only the reason differs. The test therefore proves
/// something on every platform rather than nothing on one.
#[test]
fn case_insensitive_duplicate_detection_reports_the_duplicate() {
    let root = TempTree::new("duplicates-forced");
    root.file("src/Thing.rs", "fn a() {}");
    root.file("src/thing.rs", "fn b() {}");
    let canonical = std::fs::canonicalize(root.path()).expect("canonicalize");
    let detected = CaseSensitivity::detect(&canonical);

    let options = DiscoveryOptions::default().with_case_sensitivity(CaseSensitivity::Insensitive);
    let discovery = FileDiscovery::new(root.path(), options)
        .discover()
        .expect("discover");

    assert_eq!(
        discovery.stats().files_yielded,
        1,
        "under case-insensitive comparison the two paths are one file"
    );
    if detected.is_sensitive() {
        assert_eq!(
            discovery.stats().duplicates,
            1,
            "two distinct files collided under the forced comparison, so the collision is counted"
        );
    }
    assert!(
        contains(discovery.files(), "src/Thing.rs"),
        "the retained path is the one that sorts first, so the choice is deterministic"
    );
}

/// The retained spelling is deterministic, which is why duplicate detection runs *after* sorting.
/// Filesystem read order is not stable, so "first one wins" would otherwise vary between runs.
#[test]
fn which_duplicate_is_retained_is_deterministic() {
    let root = TempTree::new("duplicates-deterministic");
    root.file("src/Thing.rs", "fn a() {}");
    root.file("src/thing.rs", "fn b() {}");
    root.file("src/THING.rs", "fn c() {}");
    let canonical = std::fs::canonicalize(root.path()).expect("canonicalize");
    if CaseSensitivity::detect(&canonical).is_insensitive() {
        eprintln!("skipping: a case-insensitive volume cannot hold three spellings of one name");
        return;
    }

    let options = DiscoveryOptions::default().with_case_sensitivity(CaseSensitivity::Insensitive);
    let first = FileDiscovery::new(root.path(), options.clone())
        .discover()
        .expect("first")
        .into_report();
    let second = FileDiscovery::new(root.path(), options)
        .discover()
        .expect("second")
        .into_report();

    assert_eq!(first.files(), second.files());
    assert_eq!(first.stats().duplicates, 2);
    assert!(
        contains(first.files(), "src/THING.rs"),
        "ASCII order puts `THING` before `Thing` before `thing`, so `THING` is retained"
    );
}

/// The aggregate statistics are derived from the yielded set, not from what the walk saw, so they
/// stay consistent with the file list after deduplication.
#[test]
fn aggregates_match_the_yielded_set() {
    let root = TempTree::new("aggregates");
    root.file("src/a.rs", &"fn a() {}\n".repeat(10));
    root.file("src/b.py", "VALUE = 1");
    root.file("src/c.go", "package main");
    root.file("README.md", "# hi");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    let stats = discovery.stats();
    let expected_bytes: u64 = discovery.files().iter().map(|file| file.size_bytes).sum();
    assert_eq!(stats.bytes, expected_bytes);
    assert_eq!(
        stats.files_yielded,
        discovery.files().len(),
        "the counter must equal the list"
    );

    let by_language: usize = stats.by_language.values().sum();
    assert_eq!(by_language, stats.files_yielded);
    assert_eq!(stats.unsupported_extension, 1, "README.md has no language");
}
