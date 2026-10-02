//! Where the repository is, and which of its paths the index holds.
//!
//! # The invariant
//!
//! The question this module answers is: *what is Peek allowed to trust, when deciding whether a
//! user-supplied path belongs to the selected repository, given that the path may not exist?*
//!
//! The answer is written out in full in `docs/path-trust.md`. In one sentence:
//!
//! > Peek trusts the repository root to be where `canonicalize` said it was. For the path the
//! > user supplied, Peek trusts only the base it was anchored to and the letters in it.
//!
//! One function implements it — [`relative_to`], through [`locate_within`]. The four cases below
//! are consequences of that one rule rather than four rules of their own, which is what they used
//! to be.
//!
//! **The rule itself lives in the engine**, in [`peek_core::containment`], because the engine asks
//! the same question in three places and used to answer it three ways. What lives here is the part
//! that is only the CLI's: anchoring a path onto a root, and turning a decision into a sentence a
//! person can act on. The comparison walk, the normalisation and the location walk are all called
//! from there, so there is nothing in this crate left to keep in step with them.
//!
//! # Containment is decided on the spelling, before the filesystem is asked
//!
//! The order is the invariant, and it is the opposite of what this module used to do. A path that
//! does not exist has no filesystem-resolved location, so a decision that asks for one first has
//! no answer for exactly the case `peek rm` exists to serve: a file deleted since the last index,
//! which must still be nameable. So:
//!
//! 1. **Anchor it.** A relative path is joined onto the *root*, never onto the working directory —
//!    `--root` has already answered which tree, and a path inside that tree must not mean
//!    different things depending on where the user is standing. The root is the single exception,
//!    because it is the only path with no other candidate to be relative to.
//! 2. **Normalise the spelling.** `.`, `//` and `..` collapse without asking the filesystem
//!    anything, and they collapse *lexically* because the platforms disagree about what `..` means:
//!    a kernel resolves it after following a link, so it can move further than it asks for, while
//!    Windows takes it off a DOS path before the filesystem is asked anything. Arithmetic here is
//!    the one reading both platforms share. On Windows a path's case is whatever the caller typed,
//!    so `C:\Repo` and `c:\repo` are different strings for one directory; the root arrives
//!    canonical and carries the on-disk case, so every comparison this module makes internally is
//!    between two on-disk spellings.
//! 3. **Test containment on the spelling.** Component-wise, never a string prefix: `/repo/src-old`
//!    is not inside `/repo/src`, and a prefix comparison is the mistake that would let one
//!    repository's index be removed by a command aimed at another's directory. This step touches
//!    no filesystem and therefore always produces an answer, including for a path that has never
//!    existed.
//! 4. **Then ask the filesystem, and only to settle what the spelling cannot.** The deepest
//!    existing prefix is resolved and re-tested, which is what catches a symlink pointing out of
//!    the tree, and which also settles whether a spelling that read as outside is one directory
//!    under another name. A resolution can add a refusal, and it can turn one refusal back into an
//!    answer; what it cannot do is answer for a path that climbs or sits on another volume, so
//!    those are refused without any I/O at all. A filesystem that cannot answer costs the
//!    refinement, not the answer.
//! 5. **Trust what is left.** The resolved prefix is inside the root and the remaining components
//!    are names. That trust is a real risk and is stated in the document, with the reason it is
//!    acceptable here and would not be for a command that writes to the filesystem.
//!
//! # A directory can be named more than one way
//!
//! **Step 3 is a necessary condition, not a sufficient one, and the reason is a fact about paths
//! rather than about this module: letters are not always a location.** The same directory has more
//! than one spelling, and on both platforms that are not different from CI the differences fall on
//! the two different sides of the prefix question.
//!
//! **On Windows** `canonicalize` answers in the extended-length spelling, `\\?\C:\repo`, so a
//! location the filesystem named and a root the caller spelled can be one directory and still fail
//! a comparison: they differ in their first component and in nothing else.
//! [`peek_core::containment`] reads a verbatim prefix as the bare prefix it stands for, which
//! settles that half. It cannot settle the other half, and the other half is ordinary:
//! `canonicalize` also expands a short name on the way to a directory, so
//! `C:\Users\RUNNER~1\repo` is `\\?\C:\Users\runneradmin\repo`
//! — one directory whose two spellings differ in a component that *does* carry a location.
//!
//! **On macOS** there is no prefix to differ and the same trouble arrives by another road: `/var` is
//! a link to `/private/var`, so every temporary directory is two spellings of one place.
//!
//! Neither is an edge case. The root [`resolve_root`] hands on is canonical and a path typed in full
//! is not, so two spellings of one location is the **ordinary** state of affairs on both, not
//! something a test had to arrange — and a gate that reads them as two directories refuses a path
//! that is inside the repository and blames a symlink for it, which is a second thing that is not
//! true.
//!
//! So step 4 is allowed to answer a refusal, where step 3 could not have been wrong about one.
//! It is asked exactly once, and only for a path that climbs nowhere and sits on the root's own
//! volume; and it can only ever answer with something it resolved, because a walk that names no
//! part of a path falls back to the spelling and therefore says what step 3 said.
//!
//! # Why one function and not one per command
//!
//! The previous shape was `canonicalize()`, then canonicalize the parent, then give up with a
//! filesystem error. That is three different notions of containment stacked in one function, which
//! is why no contract could be written for it: the answer depended on which of the three happened
//! to run. Every command that accepts a user path goes through [`relative_to`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use peek_core::containment::{
    climbs, normalise_lexically, relative_path, resolve_location, same_volume,
    windows_drive_relative,
};
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
///
/// **A drive-relative spelling is refused here**, before anything is resolved: `C:foo` names a
/// directory on a drive that this process cannot see, so it is not a root and cannot be turned
/// into one. The judgement is [`peek_core::containment`]'s, because the MCP server's `index`
/// boundary answers the same question about the same two characters, and the call site is the only
/// part that is platform-specific.
pub fn resolve_root(given: &Path, command: &'static str) -> Result<PathBuf, Failure> {
    if cfg!(windows)
        && let Some(spelling) = given.to_str()
        && windows_drive_relative(spelling)
    {
        return Err(Failure::usage(
            command,
            Refusal::new(
                exit::kind::NOT_A_REPOSITORY,
                format!(
                    "{spelling} names a location relative to whatever directory the current drive \
                     happens to be reading, which this command cannot see. Write it as an \
                     absolute path instead, with the drive and a separator between them"
                ),
            ),
        ));
    }
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

/// A path that has been placed and judged, and nothing more.
///
/// Deliberately small. The two failures this module was written to answer were both "the refusal
/// was right for the wrong reason", and a test that can only see an exit code and a sentence
/// cannot tell a contract from a filesystem error that happened to point the same way. So the
/// decision is a value, the value carries the answer and the scope, and the reasoning reaches a
/// reader through the messages below rather than through extra fields nothing reads.
struct Located {
    /// The path relative to the root, as the index knows it.
    inside: PathBuf,
    /// Whether a directory exists at that location now.
    is_directory: bool,
}

/// Express `given` relative to `root`, refusing anything outside it.
///
/// The one place containment is decided, and the only function a command needs. The decision
/// itself is [`locate_within`]; this is the translation of its answer, or of its reason, into the
/// CLI's refusal type.
///
/// A path that does not exist is still addressable — a file deleted since the last index has to be
/// nameable by the command that repairs the index, and refusing it would make that command useless
/// exactly when it is needed. That is why the decision is made on the spelling first and the
/// filesystem is asked afterwards, and only to make the answer stricter.
pub fn relative_to(root: &Path, given: &str, command: &'static str) -> Result<Relative, Failure> {
    let located = locate_within(root, given).map_err(|reason| {
        Failure::usage(
            command,
            Refusal::new(exit::kind::OUTSIDE_REPOSITORY, reason),
        )
    })?;
    // `RepoPath::new` is the model's own validator: it rejects an empty path, a NUL byte, and any
    // `..` that climbs above the root. A path that survived the containment test cannot contain a
    // leading `..`, so this is a shape check rather than a security check — the containment test
    // is the security check, and it already ran.
    let path = RepoPath::new(located.inside.to_string_lossy().as_ref()).ok_or_else(|| {
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
    Ok(Relative {
        path,
        is_directory: located.is_directory,
    })
}

/// Place `given` and decide whether it is inside `root`.
///
/// **Never reads the working directory**, so the same command line means the same thing however
/// the user is standing, and **never requires the filesystem to have an answer**, so a path that
/// has never existed is still classifiable. The refusals it returns are the whole sentence, and
/// they name the repository as well as the path: a refusal that names only the offending path
/// leaves a reader with nothing to act on.
fn locate_within(root: &Path, given: &str) -> Result<Located, String> {
    if given.is_empty() {
        return Err("the path is empty; name a file or directory inside the repository".to_owned());
    }
    if cfg!(windows) && windows_drive_relative(given) {
        return Err(format!(
            "{given} names a location relative to whatever directory the current drive happens to \
             be reading, which this command cannot see, so it cannot be shown to be inside the \
             repository at {}. Write it as an absolute path, with the drive and a separator \
             between them",
            root.display()
        ));
    }
    // The root is a precondition, not an input to be interpreted. `resolve_root` canonicalises it,
    // and a relative one here would mean the caller skipped that — at which point every
    // filesystem answer below would be relative to the process's working directory, and the same
    // command line would mean different things depending on where the user is standing.
    if !root.is_absolute() {
        return Err(format!(
            "the repository root is {}, which is not an absolute path, so this command cannot \
             tell what it is relative to. A root reaches this point only after it has been \
             resolved and canonicalised",
            root.display()
        ));
    }
    let base = normalise_lexically(root);
    let anchored = absolutise(Path::new(given), &base)?;

    // **The gate, first and on the spelling.** Component-wise, with no filesystem involved, so it
    // is available for a path that has never existed and for a volume that is not mounted. The
    // rule is [`peek_core::containment`]'s, and this module does not have a second one.
    let spelled = normalise_lexically(&anchored);
    let on_the_spelling = relative_path(&base, &spelled).is_some();

    // **And a refusal here is a decision about arithmetic, which has two kinds.** One is final: a
    // path that climbs, or one on another volume, is out of this tree by the spelling alone, so it
    // is refused here and never handed to the filesystem at all — which is what keeps a share
    // that is not answering from costing a network round trip, and a wrong path from costing I/O.
    //
    // The other is not final, because **letters are not always a location: a directory can be
    // named more than one way, and on both affected platforms the ways differ without differing in
    // place.** `canonicalize` answers in the extended-length spelling on Windows *and* expands a
    // short name on the way to a directory, so a caller who wrote `C:\Users\RUNNER~1\repo` is
    // holding the directory the root names as `\\?\C:\Users\runneradmin\repo`; and on macOS `/var`
    // is a link to `/private/var`, so every temporary directory is two spellings of one place. The
    // root [`resolve_root`] hands on is canonical and a path typed in full is not, so those are two
    // spellings of one location by default rather than by accident. The prefix exception in
    // [`peek_core::containment`] settles the first half of that and cannot settle the rest, because
    // a *name* does carry a location — so a refusal is put to the filesystem exactly once, here,
    // where the spelling could not have climbed out and the volume is the root's own.
    let worth_asking = on_the_spelling || (same_volume(&base, &spelled) && !climbs(&anchored));
    if !worth_asking {
        return Err(outside_the_repository(given, &spelled, &base));
    }

    // **The refinement.** The deepest existing prefix is resolved so a symlink is followed *before*
    // the containment test rather than after, and so the answer is the location the operating
    // system would open.
    let location = resolve_location(&anchored);
    // The root is compared in the spelling the same function gives it, for the same reason the
    // comparison is component-wise: `canonicalize` answers in the extended-length form on Windows
    // and expands a short name on the way to a directory, so a root the caller spelled and a
    // location the filesystem named would be compared as two directories where there is one. Both
    // sides now come from one function and cannot disagree about spelling.
    //
    // **Nor can they disagree about which kind of answer they are holding**, and that is a separate
    // fault this comparison had. A walk that gave up part-way used to answer with the path's own
    // spelling while the root's walk answered with a location, so one directory was compared with
    // itself in two forms — and on a runner whose temporary directory is a link (`/var` for
    // `/private/var`) that reads as the path being outside the tree and says a symlink sent it
    // there. The walk now spends a `..` instead of stopping on it, which is what leaves a location
    // on both sides or a spelling on both sides.
    let resolved_base = resolve_location(&base);

    // **One rule answers this, and it is the same rule that answered the gate.** The gate reads a
    // spelling and this reads a location; the walk between them is [`peek_core::containment`]'s,
    // so a path cannot be accepted on its spelling and then turn out to have no name inside the
    // root — which is the disagreement the previous version of this function could not rule out,
    // because it asked one question twice and wrote the walk down twice.
    let Some(inside) = relative_path(&resolved_base, &location) else {
        // Two refusals, and which one is said depends on what the spelling claimed rather than on
        // how the refusal was reached. A spelling that read as inside and resolves outside is a
        // link that left the tree, and that is what the message has to name: the spelling is
        // inside and the location is not, so only one of them can be quoted and only one of them
        // is a location.
        if on_the_spelling {
            return Err(format!(
                "{} is written as if it were inside the repository at {}, but it reaches {}, which \
                 is outside the tree. A symlink inside the repository points out of it, and this \
                 command will not follow one to a file the repository does not contain",
                spelled.display(),
                base.display(),
                location.display()
            ));
        }
        return Err(outside_the_repository(given, &spelled, &base));
    };
    Ok(Located {
        is_directory: location.is_dir(),
        inside,
    })
}

/// The refusal for a path that is not inside the repository, naming both places.
///
/// **The spelling and the repository, never the resolved location, and deliberately.** The
/// resolved location is a filesystem fact, and this refusal is reached without asking the
/// filesystem anything, so naming it here would be naming something this function never looked up.
/// What it names is where the path *points*, which is enough to tell an escape from a spelling
/// and is true without any `stat`.
fn outside_the_repository(given: &str, spelled: &Path, root: &Path) -> String {
    let mut message = format!(
        "{} is not inside the repository at {}; this command will not touch anything outside the \
         tree it was pointed at",
        spelled.display(),
        root.display()
    );
    // The spelling and where it points differ whenever `..` was involved, and a reader told only
    // one of them cannot tell an escape from a spelling. Named once when they agree, twice when
    // they do not.
    if spelled.to_string_lossy() != given {
        message.push_str(&format!(". {given} names {}", spelled.display()));
    }
    message
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
    use peek_core::containment::relative_path;
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
    fn the_root_itself_is_resolved_against_the_working_directory() {
        // The rule the default of `.` depends on, and the counterpart to
        // `a_relative_path_is_resolved_against_the_root_and_not_the_working_directory`: this is
        // the one path with nothing else to be relative to. Compared against the process's own
        // working directory rather than a guess, so the assertion is about the resolution and not
        // about a value the test invented.
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
            "a relative root is resolved against the working directory, which is the only base it \
             has; a path inside it is not"
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

    /// Say on stderr which assertions a link this environment refused to create prevented, so a
    /// caller can return rather than go on to assert nothing.
    ///
    /// Windows grants symlink creation to an elevated process or one in Developer Mode, so a test
    /// that needs a link can be *unrunnable* rather than failing, and `#[ignore]` cannot say which:
    /// an ignored test is skipped on every platform, this one is skipped only where the platform
    /// says no.
    ///
    /// **The quiet return is the failure this exists to prevent.** Every claim this module makes
    /// about a symlink is a claim about containment, so a green run that never built a link reports
    /// having checked something it did not, and nothing in the output says otherwise. The reason
    /// goes to stderr, which the harness prints for a failing run and `--nocapture` always prints —
    /// the same bar `peek-core`'s discovery tests set.
    ///
    /// **Named, not generic**, because a bare "skipping" cannot be attributed to a test from a log
    /// line, and a skipped run that says which claims did not run is the difference between a gap
    /// somebody can look for and a gap nobody can.
    fn skipped_without_a_symlink(claim: &str) {
        eprintln!(
            "skipping: this environment cannot create symlinks, so the assertions about {claim} \
             did not run (on Windows that needs Developer Mode or an elevated process)"
        );
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
        if !make_symlink(&file, &link) {
            skipped_without_a_symlink("a file link out of the tree");
            return;
        }
        let error = relative_to(&root, link.to_str().expect("utf-8"), "rm").expect_err("refused");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
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

    // -----------------------------------------------------------------------
    // The invariant, asserted. `docs/path-trust.md` states it; these are its claims.
    //
    // Several of these assert the *reason* a path is accepted or refused and not only the
    // outcome, because the two failures this module was written to answer were both "the refusal
    // was right for the wrong reason": `outside_repository` and exit code 2 were produced before
    // any of this changed, so the kind alone cannot tell a contract from a `canonicalize` that
    // happened to fail in the same direction. **An operating system error quoted in a
    // containment refusal is the tell** — it means the answer came from the filesystem rather than
    // from the rule.
    // -----------------------------------------------------------------------

    /// Make a directory symlink, and report whether this process was allowed to.
    ///
    /// A second definition for the reason given on [`make_symlink`]: Windows has a separate call
    /// for it, and an `if` compiles both arms on every platform.
    #[cfg(unix)]
    fn make_directory_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn make_directory_symlink(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    #[test]
    fn a_path_outside_the_repository_is_refused_even_when_nothing_on_it_exists() {
        // The case a filesystem cannot answer and a contract can. `elsewhere` has never been
        // created, so there is no on-disk location to compare against anything, and a check that
        // asks the filesystem first has nothing to come back with.
        //
        // The message assertions are the content of the test rather than decoration: the exit code
        // and the refusal kind were both produced before this changed. Naming the repository, and
        // not quoting an operating system error, is what tells a contract from a coincidence.
        let root = temp("trust-outside");
        let _guard = Cleanup(root.clone());
        let outside = temp("trust-outside-target");
        let _other = Cleanup(outside.clone());
        let secret = outside.join("elsewhere/secret.rs");
        assert!(
            !secret.parent().expect("the parent is named").exists(),
            "the fixture must not have created the directory the path is under, or the test is \
             asserting something the old code got right for a different reason"
        );

        let error = relative_to(&root, secret.to_str().expect("utf-8"), "rm")
            .expect_err("a path outside the root is refused whatever is on disk");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        assert!(
            error.refusal.message.contains(&root.display().to_string()),
            "the refusal must name the repository it refused to leave: {}",
            error.refusal.message
        );
        assert!(
            error.refusal.message.contains("elsewhere"),
            "the refusal must name the location it refused to touch: {}",
            error.refusal.message
        );
        assert!(
            !error.refusal.message.contains("os error"),
            "the answer must come from the containment rule, not from a filesystem call that \
             happened to fail in the same direction: {}",
            error.refusal.message
        );
    }

    #[test]
    fn a_climb_out_of_the_root_through_a_directory_that_does_not_exist_is_refused() {
        // The same escape written with a `..` and a target that is not there, because a reader of
        // the test above would try exactly that next. It is also the case that discriminates the
        // gate from the refinement: nothing on this path can be resolved, so the only thing that
        // can refuse it is arithmetic on the spelling.
        let outer = temp("trust-climb");
        let _guard = Cleanup(outer.clone());
        let root = outer.join("repo");
        std::fs::create_dir_all(&root).expect("create the repository");
        let canonical_root = root.canonicalize().expect("canonicalise");

        let error = relative_to(&canonical_root, "../nowhere/deeper/secret.rs", "rm")
            .expect_err("a climb out of the root is refused whether or not the target exists");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        assert!(
            error
                .refusal
                .message
                .contains(&canonical_root.display().to_string()),
            "the refusal must name the repository it refused to leave: {}",
            error.refusal.message
        );
        assert!(
            !error.refusal.message.contains("os error"),
            "nothing on this path exists, so a filesystem error is the only thing that could have \
             produced this answer: {}",
            error.refusal.message
        );
    }

    #[test]
    fn a_file_below_a_directory_that_does_not_exist_is_still_addressable() {
        // **The legitimate case the old walk could not reach at all**, and the reason the contract
        // had to be written down. It stripped one name and then required the parent to resolve, so
        // a path with a *missing intermediate component* was a filesystem error rather than a path:
        // `peek rm` could name a file whose directory had been deleted and could not name one
        // whose directory had never been created. Those are the same user request.
        let root = temp("trust-missing-parent");
        let _guard = Cleanup(root.clone());
        let absent = root.join("src/never/created.rs");
        assert!(
            !root.join("src").exists(),
            "the fixture must not have created src, or this is the case the old code handled"
        );

        let found = relative_to(&root, absent.to_str().expect("utf-8"), "rm")
            .expect("a path below a directory that does not exist is nameable");
        assert_eq!(found.path.as_str(), "src/never/created.rs");
        assert!(
            !found.is_directory,
            "nothing is there, so it is not a directory"
        );
    }

    #[test]
    fn a_climb_that_stays_inside_the_repository_is_resolved_and_is_addressable() {
        // `..` is arithmetic, not a question for the filesystem. A climb that ends up inside the
        // root is an ordinary path, and when the middle of it does not exist the arithmetic is the
        // only thing available.
        let root = temp("trust-climb-inside");
        let _guard = Cleanup(root.clone());
        std::fs::create_dir_all(root.join("src")).expect("create src");

        let found = relative_to(&root, "src/never/../also-never.rs", "rm")
            .expect("a climb that stays inside the root is nameable");
        assert_eq!(found.path.as_str(), "src/also-never.rs");
    }

    #[test]
    fn a_symlinked_directory_does_not_let_a_nonexistent_leaf_out_of_the_repository() {
        // **The discrimination between the two notions that survive.** Lexically,
        // `<root>/link/gone.rs` is inside the root: every component of the spelling is a name
        // under the root. It is also a file outside the root, because `link` points out of the
        // tree. An implementation that compared the spelling alone would allow it, and `peek rm`
        // would remove another tree's rows.
        //
        // The leaf is **missing on purpose**, and that is where the trust in trusted-ancestor
        // containment is paid for: nothing about `gone.rs` is checked, so the refusal has to come
        // entirely from the parent that does exist. See the risk section of `docs/path-trust.md`.
        let root = temp("trust-link-escape");
        let _guard = Cleanup(root.clone());
        let elsewhere = temp("trust-link-target");
        let _other = Cleanup(elsewhere.clone());
        let link = root.join("link");
        if !make_directory_symlink(&elsewhere, &link) {
            skipped_without_a_symlink("a directory link out of the tree with a missing leaf");
            return;
        }
        let escaped = root.join("link/gone.rs");
        let error = relative_to(&root, escaped.to_str().expect("utf-8"), "rm")
            .expect_err("a path through a symlink out of the tree is refused");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        let target = elsewhere.canonicalize().expect("canonicalise");
        assert!(
            error
                .refusal
                .message
                .contains(&target.display().to_string()),
            "the refusal must name where the link actually goes, since no component of the \
             spelling does: {}",
            error.refusal.message
        );
    }

    #[test]
    fn a_symlinked_directory_is_outside_the_repository_when_it_points_out_of_it() {
        // The whole path resolves here, so this is the case where the answer and the spelling are
        // two different places and the message has to name both. It is the shape
        // `a_symlink_pointing_outside_the_repository_is_outside_it` covers for a *file*; a
        // directory is the one that can be walked through, so it is the one that gives an escape
        // away without a single component ever naming a file outside the tree.
        let root = temp("trust-link-whole");
        let _guard = Cleanup(root.clone());
        let elsewhere = temp("trust-link-whole-target");
        let _other = Cleanup(elsewhere.clone());
        let link = root.join("link");
        if !make_directory_symlink(&elsewhere, &link) {
            skipped_without_a_symlink(
                "a directory link out of the tree under a path that resolves",
            );
            return;
        }
        let file = link.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        let error = relative_to(&root, file.to_str().expect("utf-8"), "rm")
            .expect_err("a path through a symlink out of the tree is refused");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        assert!(
            error.refusal.message.contains(&root.display().to_string()),
            "the refusal must name the repository as well as the link: {}",
            error.refusal.message
        );
    }

    #[test]
    fn a_nonexistent_file_under_a_symlinked_parent_that_stays_inside_is_addressable() {
        // The other half of the rule above: a link that leaves the tree is refused, and a link
        // that stays in it is one directory with two names, which the index holds under the name
        // the walk found. Testing only the refusal half would leave the answer for a legitimate
        // path unstated, and "symlinks are refused" is not a rule anyone should implement.
        let root = temp("trust-link-inside");
        let _guard = Cleanup(root.clone());
        std::fs::create_dir_all(root.join("real")).expect("create the directory the link names");
        let link = root.join("alias");
        if !make_directory_symlink(&root.join("real"), &link) {
            skipped_without_a_symlink("a directory link that stays inside the tree");
            return;
        }
        let found = relative_to(&root, "alias/gone.rs", "rm")
            .expect("a link that stays inside the root is a path inside the root");
        assert_eq!(
            found.path.as_str(),
            "real/gone.rs",
            "the answer is the location, not the spelling: a link and its target are one \
             directory, and the index holds it under the one the walk found"
        );
    }

    /// What a fixture writes into a file, so a test can tell which of two files it opened.
    ///
    /// The contents have to differ or the question has no answer, and `&str` rather than a
    /// `&[u8]` so that a test can match on them as patterns rather than carrying a table of
    /// filenames to compare the answer against.
    const AT_THE_ROOT: &str = "the file at the root of the repository";
    const UNDER_SRC: &str = "the file under src";

    #[test]
    fn a_climb_through_a_symlink_names_the_file_the_filesystem_would_open() {
        // Where lexical normalisation and the filesystem disagree about which *file* this is.
        // `<root>/src/link/..` is `<root>/src` to a reader of the spelling, so the two candidate
        // answers are `src/a.rs` and `a.rs`. The repository contains both, so containment cannot
        // separate them and only the filesystem can — and the index holds a file under the name the
        // walk found, so an implementation that normalised first and stopped there would remove the
        // wrong file's rows and report success.
        //
        // **Both files are created, and that is the fixture doing its job.** A fixture holding only
        // one of them would be asserting that a spelling is refused rather than that a file is
        // named, which is a different claim and a much weaker one.
        //
        // **The expected answer is read off the filesystem rather than written down, and that is
        // what makes this a test on every platform instead of on one.** The two platforms do not
        // agree about what `link/..` means, and an answer written down as `a.rs` is a statement
        // about the platform the author was on:
        //
        // - A kernel resolves `..` *after* following the link, so the climb is from the link's
        //   target and the spelling names `a.rs`. Measured on Linux and the documented behaviour
        //   of `realpath`, which is what `canonicalize` calls.
        // - Windows is given a DOS path, and the DOS path parser removes `.` and `..` while it
        //   converts the path — before the filesystem is asked anything. The link is therefore
        //   never followed and the spelling names `src\a.rs`. Measured: opening
        //   `src\link\..\a.rs` on Windows returns the contents of `src\a.rs`.
        //
        // So rather than assert one platform's resolution on all of them, this asks which file the
        // spelling opens, and requires the answer to be that file. That is the contract — the
        // *location the operating system would open* — stated in the only terms every platform can
        // be held to, and it still fails on a change that stopped following the link.
        let root = temp("trust-climb-link");
        let _guard = Cleanup(root.clone());
        std::fs::create_dir_all(root.join("src")).expect("create src");
        std::fs::create_dir_all(root.join("real")).expect("create the directory the link names");
        std::fs::write(root.join("a.rs"), AT_THE_ROOT).expect("write");
        std::fs::write(root.join("src/a.rs"), UNDER_SRC).expect("write");
        let link = root.join("src/link");
        if !make_directory_symlink(&root.join("real"), &link) {
            skipped_without_a_symlink("a climb through a directory link");
            return;
        }
        let spelling = root.join("src/link/../a.rs");
        let opened =
            std::fs::read_to_string(&spelling).expect("the filesystem opens this spelling");

        let found = relative_to(&root, spelling.to_str().expect("utf-8"), "rm")
            .expect("the climb stays inside the root");
        assert_eq!(
            found.path.as_str(),
            match opened.as_str() {
                AT_THE_ROOT => "a.rs",
                UNDER_SRC => "src/a.rs",
                other => panic!(
                    "the spelling opened a file neither fixture holds, so this test cannot say \
                     which one it names: {other:?}"
                ),
            },
            "the answer is the file this platform's filesystem opens for the spelling, and it is \
             not the same file on each: a kernel follows the link and then climbs, while Windows \
             takes the `..` off the DOS path before the filesystem is asked"
        );
    }

    #[test]
    fn a_path_reached_through_a_link_to_the_root_is_still_inside_it() {
        // **The alias case, made measurable instead of inherited.** A directory reached under a
        // second name is one directory, and the path the user typed is a real path to a file the
        // repository holds — so refusing it is the same false refusal as the prefix case, arrived
        // at by a name rather than by a prefix.
        //
        // This is the shape both platforms arrive in without help: on macOS a temporary directory is
        // `/var/...` for the caller and `/private/var/...` for the resolver because `/var` is a
        // link, and on Windows a profile with a short name is `RUNNER~1` for the caller and
        // `runneradmin` for the resolver. Neither can be relied on to be present on any given
        // runner, so a test that waited for one would assert nothing on the machines that lack it.
        // Arranging one makes the property checked everywhere, and it is a link between two
        // directories rather than anything to do with how the runner spells its own paths.
        //
        // **The link is outside the root on purpose**: inside it, the spelling would already read
        // as inside and this would be the case
        // `a_nonexistent_file_under_a_symlinked_parent_that_stays_inside_is_addressable` covers.
        // What is being asserted is that a refusal on the spelling goes to the filesystem before it
        // is believed — and that the refusal which is *not* put to the filesystem is a path that
        // climbs, which is the other test.
        let parent = temp("trust-alias-root");
        let _guard = Cleanup(parent.clone());
        let root = parent.join("repo");
        std::fs::create_dir_all(root.join("src")).expect("create the repository");
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").expect("write");
        let alias = parent.join("by-another-name");
        if !make_directory_symlink(&root, &alias) {
            skipped_without_a_symlink("a path reached through a link to the root");
            return;
        }

        let canonical_root = root.canonicalize().expect("canonicalise the repository");
        let through_the_alias = alias.join("src/a.rs");
        assert!(
            relative_path(&canonical_root, &through_the_alias).is_none(),
            "the fixture's own precondition: the spelling has to read as outside, or this asserts \
             nothing about a refusal being overturned"
        );

        let found = relative_to(
            &canonical_root,
            through_the_alias.to_str().expect("utf-8"),
            "rm",
        )
        .expect("one directory under two names is inside the repository");
        assert_eq!(
            found.path.as_str(),
            "src/a.rs",
            "the answer is the location the filesystem opens, which is in this tree; the index \
             cannot hold the alias as a path of its own, because the walk never goes through it"
        );
    }

    #[test]
    fn a_directory_on_disk_is_a_directory_and_a_path_that_is_not_there_is_not() {
        // `rm src` and `rm src/main.rs` are different claims and the answer says which one it
        // made. The scope is read from the resolved location rather than the spelling, so a
        // symlinked directory reports as the directory it is.
        let root = temp("trust-scope");
        let _guard = Cleanup(root.clone());
        std::fs::create_dir_all(root.join("src")).expect("create src");

        let directory = relative_to(&root, "src", "rm").expect("a directory in the root");
        assert!(directory.is_directory, "src is on disk and is a directory");
        let missing = relative_to(&root, "src/gone.rs", "rm").expect("a deleted file is nameable");
        assert!(!missing.is_directory, "nothing is at src/gone.rs");
    }

    #[test]
    fn a_linked_worktree_is_outside_the_repository_and_needs_no_argument() {
        // Two checkouts of one project, each with its own root and therefore its own index. Neither
        // is inside the other, and sharing a git common directory is not a reason to treat one as
        // a path in the other: containment is a statement about trees, and an index editable from
        // the wrong checkout is exactly what this prevents.
        let parent = temp("trust-worktree");
        let _guard = Cleanup(parent.clone());
        let main = parent.join("repo");
        let linked = parent.join("repo-wt");
        std::fs::create_dir_all(main.join("src")).expect("create the main checkout");
        std::fs::create_dir_all(linked.join("src")).expect("create the linked checkout");
        let in_linked = linked.join("src/a.rs");
        std::fs::write(&in_linked, "fn a() {}\n").expect("write");
        let canonical_main = main.canonicalize().expect("canonicalise");
        let canonical_linked = linked.canonicalize().expect("canonicalise");

        let error = relative_to(&canonical_main, in_linked.to_str().expect("utf-8"), "rm")
            .expect_err("a sibling worktree is outside this checkout");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);

        let error = relative_to(
            &canonical_linked,
            main.join("src/a.rs").to_str().expect("utf-8"),
            "rm",
        )
        .expect_err("and the other way round: a shared parent is not containment");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
    }

    #[test]
    fn a_relative_path_is_anchored_to_the_root_even_when_the_same_name_exists_outside_it() {
        // What a working-directory resolution would get wrong, stated so the next reader does not
        // have to reconstruct it. The decoy is the file a working-directory resolution would have
        // found for the same spelling, and the assertion is on the answer rather than on the fact
        // that a check passed: two different files can both be inside a repository.
        //
        // **This still cannot see the dangerous half of that defect, and says so.** The test
        // binary's working directory is the package directory, which is outside every fixture, so
        // a wrong resolution here would land nowhere and be refused — loudly, which is why the
        // defect read as a usage error. From *inside* the repository the wrong resolution lands on
        // a real file and nothing refuses it. The test that covers that runs the binary as a
        // subprocess; this one covers the rule.
        let parent = temp("trust-anchor");
        let _guard = Cleanup(parent.clone());
        let root = parent.join("repo");
        let decoy = parent.join("decoy");
        std::fs::create_dir_all(root.join("src")).expect("create the repository's src");
        std::fs::create_dir_all(decoy.join("src")).expect("create the decoy's src");
        std::fs::write(root.join("src/inside.rs"), "fn inside() {}\n").expect("write");
        std::fs::write(decoy.join("src/inside.rs"), "fn decoy() {}\n").expect("write");
        let canonical_root = root.canonicalize().expect("canonicalise");

        let found = relative_to(&canonical_root, "src/inside.rs", "rm")
            .expect("a relative path names a file in the root the command was pointed at");
        assert_eq!(found.path.as_str(), "src/inside.rs");
    }

    #[test]
    fn the_repository_root_itself_is_not_a_path_these_commands_will_act_on() {
        // Two boundaries, and they are the same boundary. `.` is the root, so it has no
        // repository-relative name and no scope; and a root that has not been canonicalised cannot
        // be compared against anything without guessing, which is what a relative one would force
        // — every answer below would be relative to the process's working directory. A caller that
        // skips `resolve_root` is told so rather than handed an answer.
        let root = temp("trust-root-itself");
        let _guard = Cleanup(root.clone());

        let error = relative_to(&root, ".", "rm").expect_err("the root is not a path to remove");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);

        let error = relative_to(Path::new("relative-root"), "src/a.rs", "rm").expect_err(
            "a root that is not absolute is refused rather than read against the working directory",
        );
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        assert!(
            error.refusal.message.contains("absolute"),
            "the refusal must say which property the root is missing: {}",
            error.refusal.message
        );
    }

    #[test]
    fn the_root_the_resolver_hands_on_answers_for_a_path_the_user_typed_in_full() {
        // The production shape, which no other test here had: a root that has been through
        // `resolve_root`, and so carries the filesystem's own spelling of itself, asked about a path
        // written out in full. Those are one directory and two spellings on every platform that has
        // more than one spelling for a place, and **the differences are not the same one**:
        //
        // - On Windows the prefix, which the engine's comparison reads, *and* a name, which it must
        //   not: `canonicalize` expands a short name on the way to a directory, so a root of
        //   `\\?\C:\Users\runneradmin\repo` is the directory a caller wrote
        //   `C:\Users\RUNNER~1\repo`.
        // - On macOS no prefix at all, and `/var` is a link to `/private/var`, so a temporary
        //   directory is `/var/folders/...` for the caller and `/private/var/folders/...` for the
        //   resolver.
        //
        // Comparing either pair as strings refuses a path that is inside the repository, and the
        // refusal names a symlink, which is a second thing that is not true. What answers now is
        // the filesystem, asked once — and only because this path climbs nowhere and sits on the
        // root's own volume.
        //
        // Both spellings are asserted because both are command lines someone can run, and they name
        // one file. **Where the runner's own paths carry no second spelling the two are the same
        // string and this asserts the part that holds everywhere** — that a root and a path built
        // from it answer for one file. The aliasing is not left to that, though:
        // `a_path_reached_through_a_link_to_the_root_is_still_inside_it` asserts it on every
        // platform.
        let directory = temp("resolved-root");
        let _guard = Cleanup(directory.clone());
        std::fs::create_dir_all(directory.join("src")).expect("create src");
        let file = directory.join("src/a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("write");
        let root = resolve_root(&directory, "status").expect("resolve the root");

        let relative =
            relative_to(&root, "src/a.rs", "rm").expect("a relative path inside a resolved root");
        assert_eq!(relative.path.as_str(), "src/a.rs");

        let absolute = relative_to(&root, file.to_str().expect("utf-8"), "rm")
            .expect("an absolute path inside a resolved root, spelled the way it was typed");
        assert_eq!(absolute.path.as_str(), "src/a.rs");
    }

    // The classifier itself is `peek_core::containment::windows_drive_relative`'s and is tested
    // there, on any host, because it is a judgement about two characters and the MCP server's
    // `index` boundary now answers the same question. What is tested here is this module's wiring:
    // that a drive-relative spelling reaches `relative_to` and is refused by it.

    #[cfg(windows)]
    #[test]
    fn a_drive_relative_path_is_refused_rather_than_read_against_a_drive() {
        // **This is also the one containment rule that would otherwise depend on where the user
        // is standing**, which is why it belongs with the anchoring rules rather than with the
        // Windows spelling rules. `canonicalize` on `C:src/a.rs` resolves against that drive's
        // current directory, so a path joined onto the root and then resolved would be answered
        // from process-global state nobody named.
        let root = temp("trust-drive-relative");
        let _guard = Cleanup(root.clone());

        let error = relative_to(&root, "C:src/a.rs", "rm")
            .expect_err("a drive-relative path is refused rather than guessed at");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        assert!(
            error.refusal.message.contains("drive"),
            "the refusal must say why the spelling is unanswerable: {}",
            error.refusal.message
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_unc_path_is_outside_a_drive_letter_repository_and_names_it() {
        // A share is an absolute path, so it is not refused for being unresolvable — it is refused
        // for being somewhere else, and that is the distinction the contract draws. A drive-letter
        // root cannot contain a UNC path, and a component-wise comparison says so without asking
        // the network anything: the gate runs before the filesystem is touched at all.
        let root = temp("trust-unc");
        let _guard = Cleanup(root.clone());

        let error = relative_to(&root, "\\\\no-such-share\\secret.rs", "rm")
            .expect_err("a share is outside a drive-letter root");
        assert_eq!(error.refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
        assert!(
            error.refusal.message.contains(&root.display().to_string()),
            "the refusal must name the repository it refused to leave: {}",
            error.refusal.message
        );
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
