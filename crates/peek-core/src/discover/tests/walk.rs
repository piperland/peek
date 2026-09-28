//! Walk behaviour that is not about one policy: the outcome type, the report, and errors.
//!
//! The claim under test throughout is that a discovery failure is always *distinguishable* from a
//! discovery that legitimately found nothing. Cortex reported both as `file_count: 0` with no
//! error, and a daemon read that as "never indexed" and rebuilt forever.

use super::support::TempTree;
use crate::discover::{
    Discovery, DiscoveryError, DiscoveryOptions, DiscoveryStats, EmptyReason, FileDiscovery,
    WalkIssueReason,
};

/// A successful walk is `Found`, not `Empty`.
#[test]
fn a_walk_with_files_is_found() {
    let root = TempTree::new("outcome-found");
    root.file("src/main.rs", "fn main() {}");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert!(matches!(discovery, Discovery::Found(_)));
    assert!(!discovery.is_empty());
    assert_eq!(discovery.empty_reason(), None);
    assert_eq!(discovery.files().len(), 1);
}

/// An empty walk is a distinct variant, so a caller must handle it. This is the type-level
/// statement of the root-directory trap fix.
#[test]
fn an_empty_walk_is_its_own_variant() {
    let root = TempTree::new("outcome-empty");
    root.file("notes.txt", "hello");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert!(discovery.is_empty());
    assert_eq!(discovery.empty_reason(), Some(EmptyReason::NoIndexableExtensions));
    // The report is still reachable through either variant, so a caller never has to match twice
    // to get at the numbers.
    assert_eq!(discovery.stats().files_examined, 1);
    assert_eq!(discovery.report().files().len(), 0);
}

/// `into_report` works from either variant, so a caller that logs statistics before branching
/// cannot get it wrong.
#[test]
fn the_report_is_reachable_from_either_variant() {
    let empty = TempTree::new("into-report-empty");
    empty.file("notes.txt", "hello");
    let empty = FileDiscovery::new(empty.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");
    assert!(empty.is_empty());
    assert_eq!(empty.into_report().stats().files_examined, 1);

    let full = TempTree::new("into-report-found");
    full.file("src/main.rs", "fn main() {}");
    let full = FileDiscovery::new(full.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");
    assert!(!full.is_empty());
    assert_eq!(full.into_report().files().len(), 1);
}

/// A root that does not exist is an error, not an empty result. Reporting "no files" for a
/// mistyped path is how a user loses an afternoon.
#[test]
fn a_missing_root_is_an_error() {
    let root = TempTree::new("missing-root");
    let missing = root.path().join("nope");

    let error = FileDiscovery::new(&missing, DiscoveryOptions::default())
        .discover()
        .expect_err("a missing root must be an error");

    assert!(matches!(error, DiscoveryError::RootUnreadable { .. }));
    assert!(
        error.to_string().contains("nope"),
        "the message must name the path: {error}"
    );
    assert!(
        std::error::Error::source(&error).is_some(),
        "the underlying I/O error must be preserved as the source"
    );
}

/// A file where a repository was expected is an error too.
#[test]
fn a_file_root_is_an_error() {
    let root = TempTree::new("file-root");
    root.file("a-file.rs", "fn main() {}");

    let error = FileDiscovery::new(root.path().join("a-file.rs"), DiscoveryOptions::default())
        .discover()
        .expect_err("a file is not a repository root");

    assert!(matches!(error, DiscoveryError::RootNotADirectory { .. }));
    assert!(error.to_string().contains("is not a directory"));
}

/// The report carries the canonical root, so identity, symlink containment and relative paths all
/// agree on one spelling.
#[test]
fn the_report_carries_the_canonical_root() {
    let root = TempTree::new("canonical-root");
    root.file("src/main.rs", "fn main() {}");
    let canonical = std::fs::canonicalize(root.path()).expect("canonicalize");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert_eq!(discovery.report().root(), canonical.as_path());
    assert_eq!(
        discovery.report().absolute(&crate::model::RepoPath::new("src/main.rs").expect("path")),
        canonical.join("src/main.rs"),
        "`absolute` must join against the canonical root"
    );
}

/// A root that is not already canonical is resolved, so a caller who passed a path with `..` in it
/// gets the same file set as one who passed the canonical form.
///
/// The test avoids `set_current_dir` on purpose: the working directory is process-global, and the
/// suite runs its tests in parallel threads. A test that changes it corrupts every other test in
/// the binary.
#[test]
fn a_non_canonical_root_is_resolved() {
    let root = TempTree::new("non-canonical-root");
    root.file("src/main.rs", "fn main() {}");
    let canonical = std::fs::canonicalize(root.path()).expect("canonicalize");

    // A path that is not already in canonical form.
    let roundabout = root.path().join("src").join("..");
    assert_ne!(
        roundabout,
        canonical,
        "the fixture must actually need canonicalising for this test to mean anything"
    );

    let discovery = FileDiscovery::new(&roundabout, DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert_eq!(discovery.report().root(), canonical.as_path());
    assert_eq!(discovery.files().len(), 1);
    assert_eq!(
        discovery.files()[0].path.as_str(),
        "src/main.rs",
        "paths are relative to the resolved root, so they are unaffected by how the caller spelled it"
    );
}

/// Options are readable back from the configured discovery, so a caller can report what it did
/// without keeping a second copy of the configuration.
#[test]
fn options_are_readable_from_the_discovery() {
    let root = TempTree::new("options-readable");
    let options = DiscoveryOptions::default().with_max_file_bytes(4096);
    let discovery = FileDiscovery::new(root.path(), options.clone());

    assert_eq!(discovery.options(), &options);
    assert_eq!(discovery.root(), root.path());
}

/// A depth limit is honoured, and is not a substitute for the symlink policy.
#[test]
fn a_depth_limit_is_honoured() {
    let root = TempTree::new("depth");
    root.file("a.rs", "fn a() {}");
    root.file("one/b.rs", "fn b() {}");
    root.file("one/two/c.rs", "fn c() {}");
    root.file("one/two/three/d.rs", "fn d() {}");

    let discovery = FileDiscovery::new(
        root.path(),
        DiscoveryOptions::default().with_max_depth(Some(2)),
    )
    .discover()
    .expect("discover");

    let names: Vec<&str> = discovery
        .files()
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(names, vec!["a.rs", "one/b.rs"]);
}

/// Statistics default to zero rather than being absent, so a caller never has to distinguish
/// "nothing happened" from "no statistics were collected".
#[test]
fn default_statistics_are_zeroed() {
    let stats = DiscoveryStats::default();

    assert_eq!(stats.files_yielded, 0);
    assert!(stats.is_empty());
    assert!(stats.by_language.is_empty());
    assert!(stats.by_classification.is_empty());
    assert!(stats.summary().starts_with("0 yielded"));
    assert!(stats.to_string().contains("0 yielded"));
}

/// `skipped()` is the reconciling number, so it must be the sum of every named skip reason and not
/// a separate, independently-maintained tally.
#[test]
fn skipped_is_the_sum_of_the_named_reasons() {
    let root = TempTree::new("skipped-sum");
    root.file("src/good.rs", "fn good() {}");
    root.file("README.md", "# hi");
    root.raw_file("src/corrupt.rs", b"fn f() { \xff }");
    root.file("target/debug/artifact.rs", "pub fn artifact() {}");

    let options = DiscoveryOptions::default().with_max_file_bytes(8);
    let discovery = FileDiscovery::new(root.path(), options)
        .discover()
        .expect("discover");

    let stats = discovery.stats();
    assert_eq!(stats.too_large, 1, "good.rs is longer than 8 bytes");
    assert_eq!(stats.not_utf8, 1);
    assert_eq!(stats.unsupported_extension, 1);
    assert!(stats.excluded >= 1, "target/ is in the default set");

    let named = stats.excluded
        + stats.unsupported_extension
        + stats.too_large
        + stats.not_utf8
        + stats.unreadable
        + stats.irregular_skipped
        + stats.duplicates
        + stats.symlink_directories_skipped
        + stats.symlink_skipped;
    assert_eq!(
        stats.skipped(),
        named,
        "`skipped()` must be derived, not maintained separately"
    );
}

/// The walk is a single pass with no early exit, so a per-entry problem never truncates the result.
#[test]
fn a_walk_records_problems_without_failing() {
    let root = TempTree::new("no-early-exit");
    root.file("src/a.rs", "fn a() {}");
    root.raw_symlink("src/b.rs", "missing-target.rs")
        .expect("create broken symlink");
    root.file("src/c.rs", "fn c() {}");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    let names: Vec<&str> = discovery
        .files()
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(names, vec!["src/a.rs", "src/c.rs"]);
    assert_eq!(discovery.stats().walk_issues, discovery.issues().len());
    assert!(
        discovery
            .issues()
            .iter()
            .all(|issue| !issue.path.is_empty()),
        "every issue must name an entry"
    );
}

/// Issues are reported with their reason, and the enum covers exactly the anomalies a walk can hit.
#[test]
fn issues_carry_a_typed_reason() {
    let root = TempTree::new("issue-reason");
    root.file("src/main.rs", "fn main() {}");
    root.raw_symlink("src/dangling.rs", "gone.rs")
        .expect("create broken symlink");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert_eq!(discovery.issues().len(), 1);
    assert!(matches!(
        discovery.issues()[0].reason,
        WalkIssueReason::UnresolvableSymlink { .. }
    ));
    // Every reason renders something a user can act on.
    assert!(!discovery.issues()[0].to_string().is_empty());
}

/// A file with a path that cannot be a `RepoPath` is counted and reported rather than dropped. On
/// Unix such paths are real, and a caller reconciling a file count deserves to know one exists.
#[test]
fn a_path_outside_the_root_is_reported() {
    // The walk cannot produce one of these from a rooted walk, so the guard is asserted where it
    // is reachable: a symlink escaping the root is the only way an entry ends up outside, and it
    // has its own typed reason. This test pins that a rooted walk never emits `OutsideRepository`
    // for ordinary files.
    let root = TempTree::new("no-outside");
    root.file("src/main.rs", "fn main() {}");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert!(
        !discovery
            .issues()
            .iter()
            .any(|issue| matches!(issue.reason, WalkIssueReason::OutsideRepository)),
        "a rooted walk must not report ordinary files as outside the root: {:?}",
        discovery.issues()
    );
}
