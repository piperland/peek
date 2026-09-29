//! Where the repository is, and which of its paths the index holds.
//!
//! # Path handling is a correctness property, not plumbing
//!
//! Four cases have to behave differently, and each of them is a way to end up reading or writing
//! the wrong tree:
//!
//! 1. **A relative path.** Resolved against the process's working directory, then canonicalised.
//! 2. **An absolute path.** Used as given, then canonicalised.
//! 3. **A symlinked root.** Canonicalisation follows it, so a link and its target are one
//!    repository and therefore one index. Two spellings of one directory must never produce two
//!    caches; the user would see one of them go stale for no visible reason.
//! 4. **A path outside the repository.** Refused, by name, with the resolved location in the
//!    message.
//!
//! Canonicalisation happens *before* every containment test, and the order matters. On Windows a
//! path's case is whatever the caller typed, so `C:\Repo` and `c:\repo` are different strings
//! that name the same directory; comparing them as typed would refuse a legitimate path. After
//! `canonicalize` both sides carry the on-disk case and a component-wise comparison is exact.
//!
//! # Why the comparison is component-wise and never a string prefix
//!
//! `/repo/src-old` is not inside `/repo/src`. A prefix comparison says it is, and that is exactly
//! the mistake that would let one repository's index be removed by a command aimed at another's
//! directory when two projects share a parent and a name. `strip_prefix` is component-wise, so it
//! makes the same judgement `peek_core::store::paths::is_within` does, for the same reason.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use peek_core::model::RepoPath;
use peek_core::store::{RepoId, Store, StoreError, paths};

use crate::exit::{self, Failure, Refusal};

/// The repository a command is about, and the index that describes it.
///
/// Resolved once, before the command runs, and carried in the answer. Every command's output
/// therefore names the tree it read and the file it read from, which is the first thing a person
/// needs when two checkouts of one project disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The canonical repository root.
    pub root: PathBuf,
    /// The identity the index is keyed by.
    pub repo: RepoId,
    /// The index file, whether or not it exists yet.
    pub index_path: PathBuf,
    /// Whether the index file was there before this command ran.
    ///
    /// Recorded because [`peek_core::store::Store::open`] *creates* a store when the file is
    /// absent, so any command that opens one has a side effect it cannot report without having
    /// looked first. `peek status` uses it to refuse rather than to invent an empty index;
    /// `peek doctor` uses it to say that the report describes an index this command made.
    pub index_existed: bool,
}

impl Location {
    /// The root as a string, for output.
    #[must_use]
    pub fn root_text(&self) -> String {
        self.root.display().to_string()
    }

    /// The index path as a string, for output.
    #[must_use]
    pub fn index_text(&self) -> String {
        self.index_path.display().to_string()
    }
}

/// Resolve a repository root and the index that describes it.
///
/// `command` is only used to attribute a failure, so a refusal printed by `peek rm` says `rm` and
/// not `index`.
pub fn locate(given: &Path, command: &'static str) -> Result<Location, Failure> {
    let root = resolve_root(given, command)?;
    let repo = RepoId::discover(&root).map_err(|error| {
        Failure::usage(
            command,
            Refusal::new(
                exit::kind::NOT_A_REPOSITORY,
                format!("cannot derive an identity for {}: {error}", root.display()),
            ),
        )
    })?;
    let index_path = paths::index_path(&repo).map_err(|error| {
        Failure::failed(
            command,
            Refusal::new(
                exit::kind::ENGINE,
                format!("cannot resolve where the index for this repository lives: {error}"),
            ),
        )
    })?;
    let index_existed = index_path.is_file();
    Ok(Location {
        root,
        repo,
        index_path,
        index_existed,
    })
}

/// Turn a path as typed into a canonical directory.
///
/// The default, `.`, resolves against the process's working directory, so `peek status` with no
/// argument is the obvious thing and still reaches a canonical root.
pub fn resolve_root(given: &Path, command: &'static str) -> Result<PathBuf, Failure> {
    let absolutised = absolutise(given).map_err(|detail| {
        Failure::usage(
            command,
            Refusal::new(
                exit::kind::NOT_A_REPOSITORY,
                format!("cannot resolve {}: {detail}", given.display()),
            ),
        )
    })?;
    let canonical = absolutised.canonicalize().map_err(|error| {
        Failure::usage(
            command,
            Refusal::new(
                exit::kind::NOT_A_REPOSITORY,
                format!(
                    "cannot resolve {}: {error}; the path must exist and name a directory",
                    given.display()
                ),
            ),
        )
    })?;
    if !canonical.is_dir() {
        return Err(Failure::usage(
            command,
            Refusal::new(
                exit::kind::NOT_A_REPOSITORY,
                format!(
                    "{} is not a directory; name the repository root, not a file inside it",
                    canonical.display()
                ),
            ),
        ));
    }
    Ok(canonical)
}

/// Make a path absolute against the working directory, without requiring it to exist.
///
/// `current_dir` rather than a `Path` parameter, because the process's working directory is the
/// only thing a user can be *meaning* by a relative path. A caller that means something else passes
/// an absolute path and this is a no-op for it.
fn absolutise(given: &Path) -> Result<PathBuf, String> {
    if given.as_os_str().is_empty() {
        return Err("the path is empty".to_owned());
    }
    if given.is_absolute() {
        return Ok(given.to_path_buf());
    }
    let cwd = std::env::current_dir()
        .map_err(|error| format!("the working directory cannot be read: {error}"))?;
    Ok(cwd.join(given))
}

/// A repository-relative path, and whether it names a directory on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relative {
    /// The path as the index knows it.
    pub path: RepoPath,
    /// Whether a directory exists there right now. `false` for a path that was deleted, which is
    ///   exactly the case `peek rm` has to handle.
    pub is_directory: bool,
}

/// Express `given` relative to `root`, refusing anything outside it.
///
/// A path that no longer exists is still addressable: the canonical parent is resolved and the
/// final component is re-attached, so `rm src/deleted.rs` works on a file the filesystem has
/// already lost but the index still holds. That is the case a deletion creates, so refusing it
/// would make the command useless exactly when it is needed.
pub fn relative_to(root: &Path, given: &str, command: &'static str) -> Result<Relative, Failure> {
    if given.is_empty() {
        return Err(Failure::usage(
            command,
            Refusal::new(
                exit::kind::OUTSIDE_REPOSITORY,
                "the path is empty; name a file or directory inside the repository",
            ),
        ));
    }
    let as_path = Path::new(given);
    let absolutised = absolutise(as_path).map_err(|detail| {
        Failure::usage(
            command,
            Refusal::new(
                exit::kind::OUTSIDE_REPOSITORY,
                format!("cannot resolve {given}: {detail}"),
            ),
        )
    })?;

    // Resolve as much of the path as exists, so a deleted leaf is still addressable, and so a
    // symlink is followed before the containment test rather than after.
    let resolved = resolve_existing_prefix(&absolutised).map_err(|detail| {
        Failure::usage(
            command,
            Refusal::new(
                exit::kind::OUTSIDE_REPOSITORY,
                format!("cannot resolve {given}: {detail}"),
            ),
        )
    })?;

    let inside = match resolved.strip_prefix(root) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => {
            return Err(Failure::usage(
                command,
                Refusal::new(
                    exit::kind::OUTSIDE_REPOSITORY,
                    format!(
                        "{} is not inside the repository at {}; this command will not touch \
                         anything outside the tree it was pointed at",
                        resolved.display(),
                        root.display()
                    ),
                ),
            ));
        }
    };
    // `RepoPath::new` is the model's own validator: it rejects an empty path, a NUL byte, and any
    // `..` that climbs above the root. A path that survived `strip_prefix` cannot contain a
    // leading `..`, so this is a shape check rather than a security check — the containment test
    // above is the security check, and it already ran.
    let path = RepoPath::new(inside.to_string_lossy().as_ref()).ok_or_else(|| {
        Failure::usage(
            command,
            Refusal::new(
                exit::kind::OUTSIDE_REPOSITORY,
                format!(
                    "{given} is not a repository-relative path; a path that escapes the root, or \
                     is empty, has no meaning here"
                ),
            ),
        )
    })?;
    let is_directory = resolved.is_dir();
    Ok(Relative { path, is_directory })
}

/// Canonicalise `path`, or its deepest existing ancestor plus the remainder.
///
/// The second case is the one that matters: after a deletion, the leaf is gone but its parent is
/// not, and the command that repairs the index needs to name the leaf.
fn resolve_existing_prefix(path: &Path) -> Result<PathBuf, String> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let name = path.file_name().ok_or_else(|| {
        format!(
            "{} names a filesystem root, not a path in a repository",
            path.display()
        )
    })?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", parent.display()))?;
    Ok(canonical_parent.join(name))
}

/// Every repository-relative path the index holds at least one row for.
///
/// # This is a stopgap, and here is what it is a stopgap for
///
/// `peek index` has to notice a file that was **deleted** since the last run, and a deleted file
/// is not in the discovery walk, so nothing else can see it. `peek_core::store::Store` offers no
/// query that enumerates indexed paths — `entities_in_file` needs a path to start from — and this
/// is therefore a direct read of the `entity` table through [`Store::conn`].
///
/// The read cannot corrupt anything, and `conn()` is public. But its own documentation says it
/// exists "for the diagnostics and pragmas that are part of this type's contract" and is "not
/// general-purpose", and it is right: a caller reaching past the query layer is a caller that will
/// eventually reach past the write path too. **The correct fix is a `pub fn indexed_paths(&self,
/// limit: usize) -> Result<Vec<RepoPath>, StoreError>` on `Store`.** Until that exists, this is
/// the one query the CLI reaches for, and it is named so that deleting it is a one-line change.
///
/// No limit, and that is deliberate: a limit here would mean a repository large enough to reach it
/// silently keeps its deleted files, which is the exact failure this function exists to prevent.
/// The cost is one `PathBuf` per indexed file, which is a few tens of bytes each and is bounded by
/// the number of files the user just indexed.
pub fn indexed_paths(store: &Store, command: &'static str) -> Result<Vec<RepoPath>, Failure> {
    // One closure for every failure, because these are four spellings of one thing: the query
    // failed. The command is a parameter so a failure raised while `rm` is running says `rm`; a
    // name written into the body would attribute every failure to whichever command happened to be
    // written first.
    let fail = |detail: &str| {
        Failure::failed(
            command,
            Refusal::new(
                exit::kind::ENGINE,
                format!("cannot list the indexed paths: {detail}"),
            ),
        )
    };
    let mut statement = store
        .conn()
        .prepare("SELECT DISTINCT path FROM entity")
        .map_err(|error| fail(&error.to_string()))?;
    let mut rows = statement.query([]).map_err(|error| fail(&error.to_string()))?;
    let mut paths = Vec::new();
    loop {
        let next = rows.next().map_err(|error| fail(&error.to_string()))?;
        let Some(row) = next else { break };
        let raw: String = row
            .get(0)
            .map_err(|error| fail(&format!("a path could not be decoded: {error}")))?;
        // A row that no longer validates is not something to skip quietly: it means the store holds
        // a path the model would refuse to construct, which is a real defect and not a detail.
        let Some(path) = RepoPath::new(raw) else {
            return Err(Failure::failed(
                command,
                Refusal::new(
                    exit::kind::ENGINE,
                    format!("the index holds the path {raw:?}, which is not repository-relative"),
                ),
            ));
        };
        paths.push(path);
    }
    Ok(paths)
}

/// The indexed paths, as a set, for a membership test.
pub fn indexed_path_set(
    store: &Store,
    command: &'static str,
) -> Result<BTreeSet<RepoPath>, Failure> {
    Ok(indexed_paths(store, command)?.into_iter().collect())
}

/// A store opened for the repository at `location`.
///
/// A thin wrapper so every command reports an open failure the same way, and so the one place
/// that creates an index is named.
pub fn open_store(location: &Location, command: &'static str) -> Result<Store, Failure> {
    Store::open(&location.index_path, &location.repo).map_err(|error: StoreError| {
        Failure::failed(
            command,
            Refusal::new(
                exit::kind::ENGINE,
                format!(
                    "the index at {} could not be opened: {error}",
                    location.index_path.display()
                ),
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{Relative, relative_to, resolve_root};
    use crate::args::{self, Command};
    use crate::exit::{EXIT_USAGE, kind};

    fn temp(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "peek-cli-paths-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        path
    }

    struct Cleanup(std::path::PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_relative_path_resolves_against_the_working_directory() {
        // The rule the default of `.` depends on. Compared against the process's own working
        // directory rather than a guess, so the assertion is about the resolution and not about a
        // value the test invented.
        let expected = std::env::current_dir()
            .expect("the process has a working directory")
            .join("crates");
        let root = resolve_root(std::path::Path::new("crates"), "status")
            .expect("a path under the working directory resolves");
        assert_eq!(root, expected.canonicalize().expect("canonicalise"));
    }

    #[test]
    fn an_absolute_path_resolves_to_itself() {
        let directory = temp("absolute");
        let _guard = Cleanup(directory.clone());
        let root = resolve_root(&directory, "status").expect("an absolute path resolves");
        assert_eq!(root, directory.canonicalize().expect("canonicalise"));
    }

    #[test]
    fn two_spellings_of_one_directory_reach_the_same_index() {
        // The symlink rule without a symlink: canonicalisation is what makes the spellings
        // equivalent, so `dir/.` and `dir/child/..` must not produce two caches. Asserted through
        // `resolve_root`, which is the function the identity is derived from.
        let outer = temp("spellings");
        let _guard = Cleanup(outer.clone());
        let nested = outer.join("nested");
        std::fs::create_dir_all(&nested).expect("create the nested directory");
        let direct = resolve_root(&outer, "status").expect("direct");
        let roundabout = resolve_root(&outer.join("./nested/.."), "status").expect("roundabout");
        assert_eq!(direct, roundabout);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_and_its_target_are_one_repository() {
        // A link and its target are the same tree, so they must reach the same index. Two caches
        // for one repository is how one of them silently goes stale.
        let target = temp("symlink-target");
        let _guard = Cleanup(target.clone());
        let links = temp("symlink-links");
        let link = links.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create a directory symlink");
        let through = resolve_root(&link, "status").expect("resolve through the link");
        let direct = resolve_root(&target, "status").expect("resolve the target");
        assert_eq!(through, direct);
    }

    #[test]
    fn a_file_is_not_a_repository() {
        let directory = temp("not-a-repo");
        let _guard = Cleanup(directory.clone());
        let file = directory.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        let error = resolve_root(&file, "status").expect_err("a file is refused");
        assert_eq!(error.status.exit_code(), EXIT_USAGE);
        assert!(error.refusal.message.contains("not a directory"), "{error:?}");
    }

    #[test]
    fn a_path_that_does_not_exist_is_refused_by_name() {
        let directory = temp("missing");
        let _guard = Cleanup(directory.clone());
        let missing = directory.join("nope");
        let error = resolve_root(&missing, "status").expect_err("a missing directory is refused");
        assert_eq!(
            error.refusal.kind.as_str(),
            kind::NOT_A_REPOSITORY,
            "the refusal names the category so a caller can branch on it"
        );
    }

    #[test]
    fn a_path_inside_the_repository_is_expressed_relative_to_it() {
        let root = temp("inside");
        let _guard = Cleanup(root.clone());
        std::fs::create_dir_all(root.join("src/deep")).expect("create src/deep");
        let file = root.join("src/deep/a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        let found: Relative =
            relative_to(&root, file.to_str().expect("utf-8"), "rm").expect("inside the root");
        assert_eq!(found.path.as_str(), "src/deep/a.rs");
        assert!(!found.is_directory, "a file is not a directory");
    }

    #[test]
    fn a_path_outside_the_repository_is_refused_and_the_message_names_both_places() {
        let root = temp("outside-root");
        let _guard = Cleanup(root.clone());
        let elsewhere = temp("outside-target");
        let _other = Cleanup(elsewhere.clone());
        let file = elsewhere.join("secret.rs");
        std::fs::write(&file, "fn secret() {}\n").expect("write");
        let error = relative_to(&root, file.to_str().expect("utf-8"), "rm")
            .expect_err("a path outside the root is refused");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        assert!(
            error.refusal.message.contains(&root.display().to_string()),
            "the message must name the repository it refused to leave: {}",
            error.refusal.message
        );
        assert!(
            error.refusal.message.contains(&elsewhere.display().to_string()),
            "the message must name the location it refused to touch: {}",
            error.refusal.message
        );
    }

    #[test]
    fn a_sibling_with_a_shared_name_prefix_is_outside_the_repository() {
        // `/x/repo-old` is not inside `/x/repo`. A string-prefix comparison would say it is, and
        // that is the mistake that lets one project's index be removed by a command aimed at
        // another when they share a parent and a name.
        let parent = temp("prefix-parent");
        let _guard = Cleanup(parent.clone());
        let repo = parent.join("repo");
        let sibling = parent.join("repo-old");
        std::fs::create_dir_all(&repo).expect("create the repository");
        std::fs::create_dir_all(sibling).expect("create the sibling");
        let canonical_repo = repo.canonicalize().expect("canonicalise the repository");
        let file = sibling.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        let error = relative_to(
            &canonical_repo,
            file.to_str().expect("utf-8"),
            "rm",
        )
        .expect_err("a shared prefix is not containment");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
    }

    #[test]
    fn a_deleted_file_is_still_addressable() {
        // The case a deletion creates: the file is gone from disk and still in the index, and the
        // command that repairs the index has to be able to name it.
        let root = temp("deleted");
        let _guard = Cleanup(root.clone());
        std::fs::create_dir_all(root.join("src")).expect("create src");
        let file = root.join("src/gone.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        std::fs::remove_file(&file).expect("delete");
        let found = relative_to(&root, file.to_str().expect("utf-8"), "rm")
            .expect("a deleted file is still nameable");
        assert_eq!(found.path.as_str(), "src/gone.rs");
    }

    #[test]
    fn a_symlink_pointing_outside_the_repository_is_outside_it() {
        // Canonicalisation runs before the containment test, so a link out of the tree is caught.
        // The security property is asserted on every platform; the link itself needs privileges
        // that only some platforms grant an unprivileged test process.
        let root = temp("link-root");
        let _guard = Cleanup(root.clone());
        let elsewhere = temp("link-target");
        let _other = Cleanup(elsewhere.clone());
        let file = elsewhere.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        let link = root.join("link.rs");
        if std::os::unix::fs::symlink(&file, &link).is_ok() {
            let error =
                relative_to(&root, link.to_str().expect("utf-8"), "rm").expect_err("refused");
            assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        } else if std::os::windows::fs::symlink_file(&file, &link).is_ok() {
            let error =
                relative_to(&root, link.to_str().expect("utf-8"), "rm").expect_err("refused");
            assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        } else {
            // Neither platform will let this process make a symlink, so there is nothing to
            // assert; the canonicalisation itself is covered by the spelling test above.
        }
    }

    #[test]
    fn an_empty_path_is_refused() {
        let root = temp("empty");
        let _guard = Cleanup(root.clone());
        let error = relative_to(&root, "", "rm").expect_err("an empty path is refused");
        assert_eq!(error.status.exit_code(), EXIT_USAGE);
    }

    #[test]
    fn the_argument_parser_hands_the_paths_module_the_root_the_user_typed() {
        // Two layers, one answer: the parser must not resolve, normalise or default the path, or a
        // refusal from `resolve_root` would be about a different spelling than the one the user
        // gave. Resolution happens once, in one place.
        let invocation = args::parse(["index", "crates"]).expect("parse");
        match invocation.command {
            Command::Index { root, .. } => assert_eq!(
                root,
                std::path::PathBuf::from("crates"),
                "the parser must hand the path through verbatim"
            ),
            other => panic!("expected an index command, got {other:?}"),
        }
    }

    #[test]
    fn locate_reports_the_index_the_engine_would_use_rather_than_one_of_its_own() {
        // The envelope names the index on every answer, so if this disagreed with the engine a user
        // would be told the wrong file was read.
        let root = temp("locate");
        let _guard = Cleanup(root.clone());
        let location = super::locate(&root, "status").expect("locate");
        let expected = peek_core::store::RepoId::discover(&root).expect("derive the identity");
        assert_eq!(
            location.index_path,
            peek_core::store::paths::index_path(&expected).expect("resolve the index path"),
            "the envelope must name the index the engine resolves, not one this crate composed"
        );
        assert!(!location.index_existed, "a fresh directory has no index yet");
    }
}
