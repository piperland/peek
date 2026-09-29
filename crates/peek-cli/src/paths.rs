//! Where the repository is, and which of its paths the index holds.
//!
//! # Path handling is a correctness property, not plumbing
//!
//! Four cases have to behave differently, and each of them is a way to end up reading or writing
//! the wrong tree:
//!
//! 1. **A relative path.** Resolved against the tree it belongs to, then canonicalised: a path
//!    *inside* a root the command already named is relative to that root, and a path that *names*
//!    the root is relative to the process's working directory, because the root is the one path
//!    with nothing else it could be relative to.
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
///
/// **This is the one place a relative path means "relative to where I am standing",** and it is
/// the root itself rather than something inside it. That is not an inconsistency with
/// [`absolutise`]: naming a root is a statement about the process, and a path *within* the root is
/// a statement about the tree. The distinction is that the root has no other candidate — there is
/// nothing to be relative to except the CWD — whereas a path inside a root that has been named
/// explicitly must not silently depend on where the user is standing.
pub fn resolve_root(given: &Path, command: &'static str) -> Result<PathBuf, Failure> {
    let working = std::env::current_dir().map_err(|error| {
        Failure::usage(
            command,
            Refusal::new(
                exit::kind::NOT_A_REPOSITORY,
                format!("the working directory cannot be read: {error}"),
            ),
        )
    })?;
    let absolutised = absolutise(given, &working).map_err(|detail| {
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

/// Make a path absolute, without requiring it to exist, against the tree it belongs to.
///
/// **Against the root, not the working directory.** `peek rm src/gone.rs --root /some/repo` used to
/// resolve `src/gone.rs` against the process's working directory, so one command line meant
/// different things depending on where the user was standing. From outside the repository the
/// containment check caught it and the command refused, which is why it read as a harmless usage
/// error; from a working directory *inside* the repository the same line landed on a different file
/// that was still in the tree, the check passed, and `rm` removed that one's rows. A guard that
/// only catches the escape is not a containment check.
///
/// `--root` has already answered "which tree". Once a command has named one, a relative path in
/// that command is relative to it, and [`resolve_root`] passes the working directory in as the base
/// for the root itself, which is the single path that has no other candidate.
///
/// An absolute path is returned unchanged, so this is a no-op for a caller that already anchored it.
/// A symlinked root is unaffected rather than newly special: the root arrives here already
/// canonical, which is the one spelling a link and its target share, so a relative path resolves
/// through the link exactly once and then stays anchored to the tree the index describes.
fn absolutise(given: &Path, root: &Path) -> Result<PathBuf, String> {
    if given.as_os_str().is_empty() {
        return Err("the path is empty".to_owned());
    }
    if given.is_absolute() {
        return Ok(given.to_path_buf());
    }
    Ok(root.join(given))
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
    let absolutised = absolutise(as_path, root).map_err(|detail| {
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
/// # Why the store owns this
///
/// `peek index` has to notice a file that was **deleted** since the last run, and a deleted file
/// is not in the discovery walk, so nothing else in the engine can see it. The index's own path
/// list is the only record that it was there.
///
/// An earlier version of this function prepared its own `SELECT DISTINCT path FROM entity` through
/// `Store::conn`, with a comment saying the right fix was a store method. That is what it is now:
/// `Store::indexed_paths` exists, and this is a thin translation of its error into the CLI's
/// `Failure`. `conn` documents itself as "not general-purpose", and a caller that reaches past the
/// query layer to *read* is a caller that will eventually reach past the write path to *write*.
///
/// No limit is requested, and that is deliberate: a limit here would mean a repository large
/// enough to reach it silently keeps its deleted files, which is the exact failure this function
/// exists to prevent. `Store::indexed_paths` takes a limit because a store does not know its
/// caller's intent; this call site does, and its intent is completeness.
pub fn indexed_paths(store: &Store, command: &'static str) -> Result<Vec<RepoPath>, Failure> {
    // A ceiling rather than a budget: it exists so a pathological store cannot make this
    // allocate without end, and it sits far above the number of files in any repository a person
    // has indexed. A limit sized to the caller would be a measurement, and nobody has measured
    // one.
    const CEILING: usize = 1 << 24;

    store.indexed_paths(CEILING).map_err(|error| {
        Failure::failed(
            command,
            Refusal::new(
                exit::kind::ENGINE,
                format!("cannot list the indexed paths: {error}"),
            ),
        )
    })
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
    use std::path::Path;

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
        //
        // The directory is **created here** rather than assumed. An earlier version of this test
        // resolved the relative path `crates`, which exists at the workspace root and not in the
        // package directory cargo runs tests from — so it passed or failed depending on where the
        // harness happened to be invoked from, which is a test whose result depends on the command
        // that ran it. Nothing here touches the working directory either: `set_current_dir` is
        // process-global, and one test changing it makes every other test's paths relative to
        // something it did not choose.
        let working = std::env::current_dir().expect("the process has a working directory");
        let name = "peek-cli-relative-fixture";
        let created = working.join(name);
        std::fs::create_dir_all(&created).expect("create the fixture directory");
        let _guard = Cleanup(created.clone());

        let root = resolve_root(std::path::Path::new(name), "status")
            .expect("a path under the working directory resolves");
        assert_eq!(
            root,
            created.canonicalize().expect("canonicalise"),
            "a relative path is resolved against the working directory, not the repository"
        );
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

    #[cfg(unix)]
    #[test]
    fn a_relative_path_under_a_symlinked_root_reaches_the_file_the_link_points_at() {
        // Where a lexical join and a canonicalisation disagree, and why the disagreement is
        // harmless: `resolve_root` has already followed the link by the time a path inside the
        // root is anchored, so the root it hands on is the target. The relative path is therefore
        // joined onto the tree the index describes, and a symlinked checkout reaches one
        // repository rather than indexing twice.
        let target = temp("symlink-relative-target");
        let _guard = Cleanup(target.clone());
        let links = temp("symlink-relative-links");
        let _other = Cleanup(links.clone());
        std::fs::create_dir_all(target.join("src")).expect("create src");
        std::fs::write(target.join("src/linked.rs"), "fn linked() {}\n").expect("write");
        let link = links.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create a directory symlink");

        let root = resolve_root(&link, "status").expect("resolve through the link");
        assert_eq!(root, target.canonicalize().expect("canonicalise"));
        let found = relative_to(&root, "src/linked.rs", "rm")
            .expect("a relative path under a symlinked root names the file it reaches");
        assert_eq!(found.path.as_str(), "src/linked.rs");
    }

    #[test]
    fn a_file_is_not_a_repository() {
        let directory = temp("not-a-repo");
        let _guard = Cleanup(directory.clone());
        let file = directory.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        let error = resolve_root(&file, "status").expect_err("a file is refused");
        assert_eq!(error.status.exit_code(), EXIT_USAGE);
        assert!(
            error.refusal.message.contains("not a directory"),
            "{error:?}"
        );
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
    fn a_relative_path_is_resolved_against_the_root_and_not_the_working_directory() {
        // The rule, asserted where the base is chosen. `--root` has already answered which tree,
        // so a path inside that root is relative to it; resolving it against the process's working
        // directory instead makes one command line mean different things depending on where the
        // user is standing.
        //
        // **This test cannot see the dangerous half of the defect, and says so.** The fixture is a
        // temporary directory, so the working directory of the test binary is always outside it, and
        // resolving against it fails loudly rather than landing on a real file. The case that
        // matters — a working directory *inside* the repository, where the wrong resolution still
        // passes the containment check and removes the wrong rows — needs a different working
        // directory, and `set_current_dir` is process-global. The test that covers it runs the
        // binary as a subprocess from inside the repository; this one covers the rule itself.
        let root = temp("relative-base");
        let _guard = Cleanup(root.clone());
        std::fs::create_dir_all(root.join("src")).expect("create src");
        std::fs::write(root.join("src/inside.rs"), "fn inside() {}\n").expect("write");
        let found = relative_to(&root, "src/inside.rs", "rm")
            .expect("a relative path names a file in the root the command was pointed at");
        assert_eq!(found.path.as_str(), "src/inside.rs");
        assert!(!found.is_directory, "a file is not a directory");
    }

    #[test]
    fn an_absolute_path_is_answered_by_the_root_that_contains_it_and_by_no_other() {
        // The negative claim is the one with teeth: the root does not participate. The same
        // absolute path is addressed against two repositories and answers against the one that
        // holds it, which an implementation that joined the root onto its argument could not
        // produce — `Path::join` with an absolute argument discards the base, so a join would look
        // correct here and still be wrong for a relative path.
        let holding = temp("absolute-holding");
        let _held = Cleanup(holding.clone());
        let other = temp("absolute-other");
        let _other = Cleanup(other.clone());
        std::fs::create_dir_all(holding.join("src")).expect("create src");
        let file = holding.join("src/absolute.rs");
        std::fs::write(&file, "fn absolute() {}\n").expect("write");
        let named = file.to_str().expect("utf-8");

        let inside = relative_to(&holding, named, "rm").expect("the root that holds it");
        assert_eq!(inside.path.as_str(), "src/absolute.rs");

        let outside = relative_to(&other, named, "rm")
            .expect_err("the same absolute path is outside a different root");
        assert_eq!(outside.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        let named_absolute = file.display().to_string();
        assert!(
            outside.refusal.message.contains(&named_absolute),
            "the refusal must name the absolute path as written, not something derived from \
             the root it was refused against: {}",
            outside.refusal.message
        );
    }

    #[test]
    fn a_relative_path_that_climbs_out_of_the_root_is_refused_and_names_both_places() {
        // `..` is the other way out, and anchoring on the root does not close it: `<root>/../x` is
        // outside the root exactly as `../x` was. Canonicalisation runs before the containment
        // test, so the climb is resolved on the filesystem rather than read as text, and the
        // message names where it actually landed.
        let outer = temp("climb-parent");
        let _guard = Cleanup(outer.clone());
        let root = outer.join("repo");
        let elsewhere = outer.join("elsewhere");
        std::fs::create_dir_all(&root).expect("create the repository");
        std::fs::create_dir_all(&elsewhere).expect("create the sibling");
        std::fs::write(elsewhere.join("secret.rs"), "fn secret() {}\n").expect("write");
        let canonical_root = root.canonicalize().expect("canonicalise the repository");

        let error = relative_to(&canonical_root, "../elsewhere/secret.rs", "rm")
            .expect_err("a relative path that climbs out of the root is refused");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        // Derived from the filesystem rather than from the spelling the test typed, so the
        // assertion is about the refusal and not about how this test wrote the path.
        let sibling = elsewhere.canonicalize().expect("canonicalise");
        let landed = sibling.join("secret.rs").display().to_string();
        let refused_root = canonical_root.display().to_string();
        assert!(
            error.refusal.message.contains(&refused_root),
            "the message must name the repository it refused to leave: {}",
            error.refusal.message
        );
        assert!(
            error.refusal.message.contains(&landed),
            "the message must name where the climb landed, not the spelling that was typed: {}",
            error.refusal.message
        );
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
            error
                .refusal
                .message
                .contains(&elsewhere.display().to_string()),
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
        std::fs::create_dir_all(&sibling).expect("create the sibling");
        let canonical_repo = repo.canonicalize().expect("canonicalise the repository");
        let file = sibling.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        let error = relative_to(&canonical_repo, file.to_str().expect("utf-8"), "rm")
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

    /// Make a symlink, and report whether this process was allowed to.
    ///
    /// Two definitions rather than two branches of one `if`: an `if` compiles *both* arms on
    /// every platform, so whichever platform's module does not exist here is a compile error. That
    /// is not a style point — the original version of this test failed to build on Linux for
    /// exactly this reason, having been written to run on both.
    #[cfg(unix)]
    fn make_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn make_symlink(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
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
        if make_symlink(&file, &link) {
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
        assert!(
            !location.index_existed,
            "a fresh directory has no index yet"
        );
    }
}
