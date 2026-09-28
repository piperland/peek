//! Ignore-file semantics: the defect where Cortex read no ignore file at all.
//!
//! Cortex hardcoded thirteen directory names and read nothing else, so `vendor/`, `dist/`,
//! `coverage/`, `Pods/` and every `*.min.js` in the tree went into the index. These tests assert
//! that the rules written in `tests/fixtures/mixed/.gitignore` actually take effect, including the
//! four features that hand-rolled implementations get wrong: nesting, negation, directory-only
//! patterns, and anchoring.

use super::discover_fixture;
use crate::discover::DiscoveryOptions;

use super::support::contains;

/// A directory-only rule (`/scratch/`) matches that directory and nothing of the same name deeper
/// down, because the leading slash anchors it to the ignore file's own directory.
#[test]
fn directory_only_rule_is_anchored_to_the_root() {
    let discovery = discover_fixture();

    assert!(
        !contains(discovery.files(), "scratch/scratch.py"),
        "/scratch/ must exclude the directory it names"
    );
    assert!(
        contains(discovery.files(), "src/scratch/helper.py"),
        "an anchored directory rule must not exclude a same-named directory at another depth"
    );
}

/// A file glob with no slash matches at any depth, which is git's rule and the reason a naive
/// `starts_with` implementation gets nested trees wrong.
#[test]
fn pattern_without_a_slash_matches_at_any_depth() {
    let discovery = discover_fixture();

    assert!(
        !contains(discovery.files(), "deep/anywhere.py"),
        "a slashless pattern must match a nested file"
    );
    assert!(
        contains(discovery.files(), "deep/anywhere_helper.py"),
        "a slashless pattern must not match a file whose name merely starts the same way"
    );
}

/// An anchored pattern matches only at the root.
#[test]
fn anchored_pattern_does_not_match_at_depth() {
    let discovery = discover_fixture();

    let yielded: Vec<&str> = discovery
        .files()
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert!(
        !contains(discovery.files(), "toponly.rs"),
        "/toponly.rs must exclude the root-level file; yielded {yielded:?}"
    );
    // The deeper file is named `toponly` — the same name as the root one, which is the whole
    // point of the rule. An earlier version of this test spelled it `touponly`, which asserted
    // the existence of a file that is not in the fixture and so failed for a reason that had
    // nothing to do with anchoring.
    assert!(
        contains(discovery.files(), "deep/toponly.rs"),
        "/toponly.rs must not exclude a deeper file of the same name; yielded {yielded:?}"
    );
}

/// Negation is the feature with the most subtle semantics: the last matching rule wins, and a file
/// is re-included only if no parent *directory* was excluded. `*.generated.rs` excludes both
/// candidates; `!keep.generated.rs` puts one back.
#[test]
fn negation_reincludes_a_file_excluded_by_an_earlier_rule() {
    let discovery = discover_fixture();

    assert!(
        !contains(discovery.files(), "src/api.generated.rs"),
        "the glob must exclude every match"
    );
    assert!(
        contains(discovery.files(), "keep.generated.rs"),
        "a later negation must re-include the file it names"
    );
}

/// A nested `.gitignore` applies below its own directory and nowhere else. This is the rule that a
/// single flat list of patterns cannot express, and it is the reason Peek uses the `ignore` crate.
#[test]
fn nested_ignore_file_is_scoped_to_its_own_directory() {
    let discovery = discover_fixture();

    assert!(
        !contains(discovery.files(), "src/nested/local.rs"),
        "a nested ignore file must exclude files below itself"
    );
    assert!(
        contains(discovery.files(), "src/local.rs"),
        "a nested ignore file must not exclude a same-named file above itself"
    );
}

/// `.ignore` is read alongside `.gitignore`. Both are committed so the test proves two files are
/// honoured rather than one.
#[test]
fn dot_ignore_file_is_honoured_alongside_gitignore() {
    let discovery = discover_fixture();

    assert!(
        !contains(discovery.files(), "editor/notes.py"),
        ".ignore must exclude the directory it names"
    );
}

/// Ignore files apply even though the fixture is not a git repository.
///
/// This is a deliberate divergence from git, and the fixture exists to hold it up: `require_git`
/// is `false` so an agent pointed at a plain directory tree gets the same answer as one pointed at
/// a checkout.
#[test]
fn ignore_files_apply_outside_a_git_repository() {
    let root = super::support::mixed_fixture();
    assert!(
        !root.join(".git").exists(),
        "this test is only meaningful if the fixture is not a repository"
    );

    let discovery = discover_fixture();
    assert!(
        !contains(discovery.files(), "toponly.rs"),
        "a .gitignore must be honoured with no .git directory present"
    );
}

/// Turning ignore files off must be observable, or the option is decorative. With them off, a file
/// the `.gitignore` excluded appears, while the exclusion *set* still applies.
#[test]
fn ignore_files_can_be_disabled() {
    let options = DiscoveryOptions::default().with_ignore_files(false);
    let discovery = super::discover_fixture_with(options);

    assert!(
        contains(discovery.files(), "toponly.rs"),
        "with ignore files disabled, a .gitignore rule must not apply"
    );
    assert!(
        !contains(discovery.files(), "target/debug/tool.rs"),
        "the exclusion set is independent of ignore files and must still apply"
    );
}
