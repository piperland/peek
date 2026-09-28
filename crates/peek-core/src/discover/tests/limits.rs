//! The size cap and the non-UTF-8 guard.
//!
//! Cortex fed a 200 MB file straight to the parser with no cap, and propagated a single non-UTF-8
//! file's decode failure out of `build_full` so that one file destroyed an entire repository's
//! index and persisted nothing. Both defects are about the same failure mode: a file that cannot
//! be processed takes the whole repository down with it. Here both are local, and both are counted.

use super::discover_fixture;
use super::support::{TempTree, contains};
use crate::discover::{DiscoveryOptions, FileDiscovery, WalkIssueReason};

/// A file above the cap is skipped and counted, never read.
///
/// The check is on the stat'd size *before* the read, so a 200 MB file costs one `stat` rather than
/// 200 MB of I/O — which is the only way a cap is worth having.
#[test]
fn a_file_above_the_cap_is_skipped_and_counted() {
    let root = TempTree::new("size-cap");
    root.file("small.rs", "fn a() {}");
    root.file("big.rs", &"// padding\n".repeat(4096));

    let options = DiscoveryOptions::default().with_max_file_bytes(1024);
    let discovery = FileDiscovery::new(root.path(), options)
        .discover()
        .expect("discover");

    assert!(contains(discovery.files(), "small.rs"));
    assert!(!contains(discovery.files(), "big.rs"));
    assert_eq!(
        discovery.stats().too_large,
        1,
        "an oversized file must be counted, not silently dropped"
    );
}

/// The cap is a cap, not a rule of thumb: a file exactly at the limit is indexed.
#[test]
fn a_file_exactly_at_the_cap_is_included() {
    let root = TempTree::new("size-boundary");
    let contents = "fn a() {}"; // 9 bytes
    root.file("exact.rs", contents);

    let at_limit = DiscoveryOptions::default().with_max_file_bytes(contents.len() as u64);
    let included = FileDiscovery::new(root.path(), at_limit)
        .discover()
        .expect("discover");
    assert!(
        contains(included.files(), "exact.rs"),
        "a file of exactly the cap size must be indexed"
    );

    let over_limit = DiscoveryOptions::default().with_max_file_bytes(contents.len() as u64 - 1);
    let excluded = FileDiscovery::new(root.path(), over_limit)
        .discover()
        .expect("discover");
    assert!(!contains(excluded.files(), "exact.rs"));
    assert_eq!(excluded.stats().too_large, 1);
}

/// The default cap is finite. Cortex's was infinity, which is the defect.
#[test]
fn the_default_cap_is_finite_and_documented() {
    let cap = DiscoveryOptions::default().max_file_bytes();
    assert!(cap > 0, "a zero default would index nothing");
    assert!(
        cap <= 16 * 1024 * 1024,
        "the default cap must be well under the sizes that stall a parse, got {cap}"
    );
}

/// The regression test for the headline defect: one undecodable file does not abort a repository.
///
/// The fixture contains `src/corrupt.rs`, a `.rs` file whose bytes are not valid UTF-8, next to
/// ordinary source. Cortex's behaviour was `?` on the decode, which aborted the whole index.
#[test]
fn one_non_utf8_file_does_not_abort_the_repository() {
    let root = TempTree::new("non-utf8");
    root.raw_file("src/good.rs", "fn good() {}");
    root.raw_file("src/also_good.py", "VALUE = 1");
    root.raw_file(
        "src/corrupt.rs",
        b"fn main() { // \xff\xfe not utf-8\n}\n",
    );
    root.raw_file(
        "src/binary.go",
        &[0x00, 0x01, 0x02, 0xff, 0xfe, 0x03],
    );

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discovery must not fail because of a bad file");

    assert!(
        contains(discovery.files(), "src/good.rs"),
        "a good file next to a bad one must still be indexed"
    );
    assert!(contains(discovery.files(), "src/also_good.py"));
    assert!(!contains(discovery.files(), "src/corrupt.rs"));
    assert!(!contains(discovery.files(), "src/binary.go"));

    assert_eq!(
        discovery.stats().not_utf8,
        2,
        "both undecodable files must be counted"
    );
    assert_eq!(discovery.stats().files_yielded, 2);
}

/// Everything after a bad file in the walk is still reached, so the failure is not merely
/// non-fatal but localised.
#[test]
fn a_bad_file_does_not_stop_the_walk() {
    let root = TempTree::new("non-utf8-locality");
    for index in 0..20 {
        root.file(&format!("src/file{index:02}.rs"), "fn f() {}");
    }
    // A name that sorts after every other entry, so a walker that aborted on it would yield 19
    // files instead of 20.
    root.raw_file("src/zzz_corrupt.rs", b"fn f() { \xff }");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert_eq!(
        discovery.stats().files_yielded,
        20,
        "every file after the bad one must still be reached"
    );
}

/// Discovery must not hand the extractor something that will fail.
///
/// This is the requirement behind the guard, and it is a stronger claim than "we skip some bad
/// files": *every* yielded file is guaranteed decodable, so the extractor's decode cannot be the
/// thing that fails.
#[test]
fn every_yielded_file_is_decodable() {
    let discovery = discover_fixture();

    for file in discovery.files() {
        let bytes = std::fs::read(discovery.report().absolute(&file.path))
            .unwrap_or_else(|error| panic!("{} became unreadable: {error}", file.path));
        assert!(
            std::str::from_utf8(&bytes).is_ok(),
            "{} was yielded but does not decode, so the extractor would fail on it",
            file.path
        );
    }
    // And the fixture really does contain one, so the assertion above is not vacuous.
    assert!(
        discovery.stats().not_utf8 >= 1,
        "the fixture contains src/corrupt.rs, so the loop above is checking a real filter"
    );
}

/// A directory named like a source file is not indexed, and does not abort the walk.
///
/// A `.rs`-named directory is an odd thing to find, and it is the portable way to produce an entry
/// the walker hands back as something other than a regular file. Permissions cannot be relied on
/// here: the suite runs as root on Linux, where `chmod 000` changes nothing.
#[test]
fn a_directory_named_like_a_source_file_is_not_indexed() {
    let root = TempTree::new("irregular");
    root.file("src/good.rs", "fn good() {}");
    root.dir("src/thing.rs");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert!(contains(discovery.files(), "src/good.rs"));
    assert!(
        !contains(discovery.files(), "src/thing.rs"),
        "a directory is not a file however it is named"
    );
    assert_eq!(
        discovery.stats().directories_visited,
        3,
        "the root, `src`, and `src/thing.rs` are all directories"
    );
}

/// A walk issue names the entry and the reason, so `doctor` can point at a specific path.
#[test]
fn an_unresolvable_symlink_issue_names_its_entry() {
    let root = TempTree::new("issue-detail");
    root.file("src/main.rs", "fn main() {}");
    root.raw_symlink("src/dangling.rs", "nowhere.rs")
        .expect("create broken symlink");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    let issue = discovery
        .issues()
        .iter()
        .find(|issue| issue.path == "src/dangling.rs")
        .expect("the broken symlink must be reported by path");
    assert!(matches!(
        issue.reason,
        WalkIssueReason::UnresolvableSymlink { .. }
    ));
    assert!(
        issue.to_string().contains("src/dangling.rs"),
        "the display form must name the path: {issue}"
    );
}

/// An indexable file that is empty is still indexed. An empty file is not an error, and a
/// `not_utf8`-style guard must not accidentally exclude it.
#[test]
fn an_empty_source_file_is_indexed() {
    let root = TempTree::new("empty-file");
    root.raw_file("src/empty.rs", b"");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert!(contains(discovery.files(), "src/empty.rs"));
    assert_eq!(discovery.stats().files_yielded, 1);
    assert_eq!(discovery.stats().bytes, 0);
}

/// When a cap and a decode failure would both apply, the size check runs first, so the counts stay
/// unambiguous: an oversized undecodable file is `too_large`, not `not_utf8`.
#[test]
fn the_size_check_runs_before_the_decode() {
    let root = TempTree::new("both-fail");
    root.raw_file("src/both.rs", &vec![0xffu8; 4096]);

    let options = DiscoveryOptions::default().with_max_file_bytes(16);
    let discovery = FileDiscovery::new(root.path(), options)
        .discover()
        .expect("discover");

    let stats = discovery.stats();
    assert_eq!(stats.too_large, 1);
    assert_eq!(stats.not_utf8, 0, "an unread file is not a decode failure");
}

/// The `Discovery::Empty` variants are distinguishable, which is the requirement behind
/// `EmptyReason`. A walk that finds nothing must never be reported as an ordinary success.
#[test]
fn an_empty_walk_reports_a_reason() {
    let root = TempTree::new("empty-tree");
    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert!(discovery.is_empty());
    assert_eq!(
        discovery.empty_reason(),
        Some(crate::discover::EmptyReason::NoFiles)
    );
    assert_eq!(discovery.files().len(), 0);
}

/// ...and the reason is specific enough to act on.
#[test]
fn a_tree_of_unsupported_files_says_so() {
    let root = TempTree::new("unsupported-only");
    root.file("README.md", "# hi");
    root.file("data.json", "{}");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert_eq!(
        discovery.empty_reason(),
        Some(crate::discover::EmptyReason::NoIndexableExtensions),
        "two markdown and json files is not the same finding as an empty directory"
    );
    assert_eq!(discovery.stats().unsupported_extension, 2);
}

/// A tree where every indexable file is unusable is a third, distinct finding.
#[test]
fn a_tree_of_unusable_files_says_so() {
    let root = TempTree::new("unusable-only");
    root.raw_file("src/a.rs", &vec![0xffu8; 128]);
    root.file("notes.md", "# hi");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert_eq!(
        discovery.empty_reason(),
        Some(crate::discover::EmptyReason::AllUnusable),
        "an indexable file existed and was rejected, which is a different problem from having none"
    );
}

/// The reported reason is always accompanied by the full statistics, so an empty result is
/// diagnosable rather than merely absent. This is the whole point of the `Empty` variant.
#[test]
fn an_empty_result_still_carries_statistics() {
    let root = TempTree::new("empty-with-stats");
    root.file("README.md", "# hi");

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    let report = discovery.report();
    assert_eq!(report.stats().files_examined, 1);
    assert_eq!(report.stats().unsupported_extension, 1);
    assert_eq!(report.stats().entries_visited, 2, "the root and the file");
}
