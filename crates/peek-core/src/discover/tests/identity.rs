//! Repository identity and worktree isolation.
//!
//! Cortex had no git awareness at all, so `with_store_path` let two worktrees share one store: each
//! overwrote the other's `repo_path` and the whole document set, and neither ever reported an
//! error. These tests build a **real** repository with a **real** linked worktree, because asserting
//! that two worktrees get different ids requires actually having two.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::support::TempTree;
use crate::discover::{DiscoveryOptions, FileDiscovery, RepoId, RepoIdSource, RepoIdentity};

/// Whether the `git` CLI is usable in this environment.
///
/// A skip here is loud. These tests cannot be faked, and a test that quietly does nothing is worse
/// than one that says it did nothing.
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Whether `path` is genuinely outside any git repository, from git's own point of view.
///
/// The tests that assert the path-only fallback depend on `git rev-parse` failing for the temporary
/// tree. Some CI runners place their temp directory *inside* a checkout, where git walks upwards
/// and finds a repository. Rather than let those tests fail for a reason that has nothing to do with
/// the code under test, they skip loudly.
fn outside_any_repository(path: &Path) -> bool {
    if !git_available() {
        return false;
    }
    Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--git-dir")
        .output()
        .map(|output| !output.status.success())
        .unwrap_or(false)
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?} could not be run: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with one commit, plus a linked worktree beside it.
///
/// Holds its [`TempTree`] so the checkout outlives the function that created it.
struct Repository {
    /// Kept alive for `Drop`, never read.
    _root: TempTree,
    /// The primary checkout.
    main: PathBuf,
    /// The linked worktree, a different directory of the same repository.
    worktree: PathBuf,
}

fn repository_with_worktree() -> Option<Repository> {
    if !git_available() {
        eprintln!("skipping: the `git` CLI is not available in this environment");
        return None;
    }

    let root = TempTree::new("repo");
    let main = root.path().join("main");
    let worktree = root.path().join("feature");
    let parent = root.path().to_path_buf();

    std::fs::create_dir_all(&main).expect("create main checkout");
    // `init` without `--initial-branch`, so the test does not depend on git >= 2.28. The initial
    // branch's name is irrelevant: the assertions are about paths and the common directory.
    run_git(&main, &["init", "--quiet"]);
    // A fixed identity keeps the commit from depending on the machine's global git config, which
    // may have no user configured at all.
    run_git(
        &main,
        &["config", "user.email", "discovery@example.invalid"],
    );
    run_git(&main, &["config", "user.name", "Discovery Test"]);
    std::fs::write(main.join("lib.rs"), "pub fn shared() {}\n").expect("write source");
    run_git(&main, &["add", "lib.rs"]);
    run_git(&main, &["commit", "--quiet", "-m", "initial"]);

    // The worktree path is resolved against the directory git is run from, which `-C` sets, so the
    // relative path lands beside the primary checkout rather than in the process's directory.
    run_git(
        &parent,
        &["worktree", "add", "--quiet", "-b", "feature", "feature"],
    );

    Some(Repository {
        _root: root,
        main,
        worktree,
    })
}

/// The headline requirement: two worktrees of one repository get different ids, so they cannot
/// share a store.
#[test]
fn two_worktrees_of_one_repository_get_different_ids() {
    let Some(repository) = repository_with_worktree() else {
        return;
    };

    let main = RepoIdentity::identify(&fs_canonical(&repository.main));
    let feature = RepoIdentity::identify(&fs_canonical(&repository.worktree));

    assert_ne!(
        main.id(),
        feature.id(),
        "two worktrees must not be able to share a store: {} vs {}",
        main.id(),
        feature.id()
    );
}

/// The reason the *common directory* is not sufficient on its own, asserted rather than asserted
/// in prose. Two worktrees share one common directory, so an id derived from it alone would
/// collide — which is why the id is a function of the root *and* the common directory.
#[test]
fn the_common_directory_alone_cannot_separate_worktrees() {
    let Some(repository) = repository_with_worktree() else {
        return;
    };

    let main = RepoIdentity::identify(&fs_canonical(&repository.main));
    let feature = RepoIdentity::identify(&fs_canonical(&repository.worktree));

    assert_eq!(
        main.source(),
        RepoIdSource::GitCommonDir,
        "both checkouts are inside a repository, so git must have been consulted"
    );
    assert_eq!(
        main.common_dir(),
        feature.common_dir(),
        "a linked worktree shares its parent repository's common directory, which is exactly why \
         the root has to be part of the identity"
    );
}

/// The positive case: the common directory is the same for both, so it identifies the *repository*,
/// while the root identifies the *checkout*. Both are folded in, and both are recoverable.
#[test]
fn the_common_directory_identifies_the_repository() {
    let Some(repository) = repository_with_worktree() else {
        return;
    };

    let main = RepoIdentity::identify(&fs_canonical(&repository.main));
    let feature = RepoIdentity::identify(&fs_canonical(&repository.worktree));

    // The common directory is the same for a repository and all of its worktrees. That is exactly
    // why it cannot be what separates them, and why the identity hashes the root as well.
    assert_eq!(
        main.common_dir(),
        feature.common_dir(),
        "a worktree shares the repository's common directory"
    );

    let common = main
        .common_dir()
        .expect("a repository must have a common directory");
    assert!(
        common.ends_with(".git"),
        "the resolved common directory is the repository's .git: {}",
        common.display()
    );
    assert!(
        common.is_absolute(),
        "a relative answer would make two worktrees hash identically, which is the collision this \
         resolution exists to prevent: {}",
        common.display()
    );
}

/// The same checkout always gets the same id, or a store would be orphaned on every run.
#[test]
fn the_same_checkout_always_gets_the_same_id() {
    let Some(repository) = repository_with_worktree() else {
        return;
    };

    let root = fs_canonical(&repository.main);
    assert_eq!(
        RepoIdentity::identify(&root).id(),
        RepoIdentity::identify(&root).id()
    );
}

/// A path that is not a repository falls back to a root-only hash and **says so**, because the
/// fallback is materially weaker and a caller that cares has to be able to see it.
#[test]
fn a_non_repository_falls_back_to_a_path_hash_and_reports_it() {
    let root = TempTree::new("not-a-repo");
    root.file("src/main.rs", "fn main() {}");
    if !outside_any_repository(root.path()) {
        eprintln!("skipping: the temporary directory is inside a git repository on this machine");
        return;
    }
    let canonical = fs_canonical(root.path());

    let identity = RepoIdentity::identify(&canonical);
    assert_eq!(
        identity.source(),
        RepoIdSource::Path,
        "a plain directory has no common directory, so the fallback must be reported"
    );
    assert!(identity.common_dir().is_none());
    assert!(identity.id().as_str().starts_with("p:"));
    assert_eq!(identity.id().as_str().len(), "p:".len() + 16);
}

/// The one-character tag in the id and the reported source are the same fact, stored once. A caller
/// parsing the prefix and a caller reading `RepoId::source` must never be able to disagree.
#[test]
fn the_id_tag_and_the_reported_source_agree() {
    for (common_dir, expected_tag, expected_source) in [
        (None, "p", RepoIdSource::Path),
        (
            Some(PathBuf::from("/repo/.git")),
            "g",
            RepoIdSource::GitCommonDir,
        ),
    ] {
        let id = RepoId::new(Path::new("/repo"), common_dir.as_deref());
        assert_eq!(id.source(), expected_source);
        assert!(
            id.as_str().starts_with(expected_tag),
            "the tag in {id} disagrees with its reported source {expected_source}"
        );
    }
}

/// The fallback is still sound for the worktree case it exists to cover: two different checkouts
/// on the same volume get different ids.
#[test]
fn the_path_fallback_still_separates_two_checkouts() {
    let root = TempTree::new("not-a-repo-two");
    if !outside_any_repository(root.path()) {
        eprintln!("skipping: the temporary directory is inside a git repository on this machine");
        return;
    }
    let first = root.path().join("one");
    let second = root.path().join("two");
    for directory in [&first, &second] {
        std::fs::create_dir_all(directory).expect("create checkout");
        std::fs::write(directory.join("main.rs"), "fn main() {}").expect("write source");
    }

    let first = RepoIdentity::identify(&fs_canonical(&first));
    let second = RepoIdentity::identify(&fs_canonical(&second));
    assert_ne!(first.id(), second.id());
}

/// The id is a fixed-width hex string, so it is safe in a store directory name on every platform,
/// and the source tag makes a key self-describing.
#[test]
fn the_id_is_a_fixed_width_self_describing_string() {
    let root = TempTree::new("id-shape");
    root.file("main.rs", "fn main() {}");
    let id = RepoId::new(&fs_canonical(root.path()), None);

    assert_eq!(
        id.as_str().len(),
        18,
        "one tag character, a colon, and 16 hex digits"
    );
    assert!(id.as_str().starts_with("p:"));
    assert!(
        id.as_str()[2..].chars().all(|c| c.is_ascii_hexdigit()),
        "the hash must be lowercase hex: {}",
        id.as_str()
    );
}

/// The hash input is length-delimited, so two different (root, common) pairs cannot be made to
/// collide by moving a path separator between them.
#[test]
fn the_hash_delimits_its_inputs() {
    let a = RepoId::new(Path::new("/x/ab"), Some(Path::new("/c")));
    let b = RepoId::new(Path::new("/x/a"), Some(Path::new("/bc")));
    assert_ne!(a, b, "a boundary shift must not produce the same id");
}

/// A walk reports the identity it used, so a caller never has to recompute it — and therefore never
/// has a chance of computing a different one.
#[test]
fn a_walk_reports_the_identity_it_used() {
    let root = TempTree::new("walk-identity");
    root.file("src/main.rs", "fn main() {}");
    let canonical = fs_canonical(root.path());

    let discovery = FileDiscovery::new(root.path(), DiscoveryOptions::default())
        .discover()
        .expect("discover");

    assert_eq!(
        discovery.repo().id(),
        RepoIdentity::identify(&canonical).id()
    );
    assert_eq!(
        discovery.report().root(),
        canonical.as_path(),
        "the report must carry the canonical root, so identity and containment agree"
    );
}

fn fs_canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).expect("canonicalize")
}
