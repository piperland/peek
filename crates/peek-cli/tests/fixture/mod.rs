//! A real repository on disk, built for one test, with its own index.
//!
//! # Why the fixture writes actual files
//!
//! Every test in this suite runs the real indexer over a real tree. That is deliberate and it
//! costs: a synthetic graph would be faster and would test less. The things this crate must get
//! right — a deleted file being noticed, a path outside the repository being refused, the counts
//! in `status` matching the store's own — are all properties of the interaction between
//! discovery, extraction and storage. A fixture that mocked any of them would be testing the mock.
//!
//! # Why the index directory is overridden globally
//!
//! `peek_core::store::paths` resolves the index from a **process-global** root, and
//! `set_root_override` is the documented way to point it somewhere a test can delete. The
//! consequence is that two tests cannot be reading two different index roots at the same time, so
//! the lock below serialises them. That is a real cost and it is paid on purpose: the alternative
//! is a test that reaches the developer's own index, which is a far worse surprise.
//!
//! **The override is installed for the length of one run, not the life of a repository.** Holding
//! it for the repository's lifetime looked like the stronger guarantee and was the wrong shape
//! twice over. It made two `Repository` values in one test a deadlock, because `std::sync::Mutex`
//! is not reentrant and the second constructor waited on a guard the first one still held. And a
//! repository that installs the override *also* silently reassigns it: with two alive, the
//! second one's cache became the process-wide root and the first one's commands wrote into it,
//! so a test asserting that two repositories have separate indexes was one `set_root_override`
//! away from asserting nothing. Scoping the override to the run removes both, and the ordering it
//! used to have to get right — restore the default before the directory is removed — is gone with
//! it, because the directory is only reachable while a guard is held.
//!
//! # Why the repository is injected into every command line
//!
//! `--root` defaults to `.` and cargo runs an integration test binary with its working directory
//! at the package directory, so `run(&repository, &["status"])` answered about `crates/peek-cli`.
//! There was an index there to be found, or rather none, so the command refused with `no_index`
//! and the test read as a wrong answer type rather than a missing flag. Fourteen tests were
//! checking a real behaviour against a directory that is not a fixture. [`run`] therefore names
//! the repository on every command line, and checks afterwards that it did.
//!
//! **That check has three answers and used to have two.** A named root is the fixture's, a named
//! root is not, and a root the filesystem will not resolve is neither — and the third was reported
//! as the second. A test that spells its root as `<root>/src/..` is naming the fixture, so on a
//! platform whose `canonicalize` cannot answer about that spelling whole the check failed in the
//! harness for a command that was pointed exactly where it was asked to point. Both halves of the
//! comparison now come from one function, and a root the filesystem declines to locate is named as
//! unlocated rather than as misdirected.
//!
//! # Why there are two spellings of a repository's root
//!
//! **A test that spells its root through a climb has to write the root the way a person would,
//! and that is not `canonicalize`'s answer.** On Windows the canonical answer carries a `\\?\`
//! prefix, a verbatim path reaches the filesystem exactly as written, and so nothing ever takes a
//! `..` off one: a climb written after it names nothing at all, for the command exactly as for the
//! harness. [`Repository::spelled`] is that other spelling, and on Unix it *is*
//! [`Repository::root`] — so a test building a pair of spellings out of it builds one on every
//! platform instead of inheriting whatever the runner's temporary directory happens to look like.
//!
//! # The fixture's own surface
//!
//! A small, fixed Rust tree with one file per language feature that produces a relation the query
//! surface needs: a function that calls another, a type with a method, a bare name, and a name that
//! two files both declare so that ambiguity is reachable without inventing an index by hand.
//!
//! # Why a direct caller goes through [`run_direct`]
//!
//! `--index-dir` is read by the binary's `main`, not by [`peek_cli::run`], so a library caller has
//! exactly one lever on where the index is resolved from: the process-global override. That makes
//! the scoping a property of the *call* rather than of the command line, which no amount of
//! `--root` can substitute for — and substituting it is exactly what a plausible-looking fix
//! would have done, on a command line that already named the repository correctly. So the fixture
//! scopes every run it starts and names the repository on the command line as well, and a test
//! that drives the library itself goes through [`run_direct`] rather than handing an invocation
//! straight to `peek_cli::run`. A test that holds [`with_index_root`] itself — `watch.rs` and
//! `end_to_end.rs` do, each for the parsed invocation or the sink's lines it needs afterwards — is
//! holding it for the length of one run in the same way, and says so where it does.

// `expect` and `panic` are denied workspace-wide; an integration test is a separate crate and does
// not inherit the library's `cfg_attr(test, allow(..))`, so it is exempted here for the same reason
// `real_repository.rs` is. A failure inside this module means the fixture could not be built, and
// continuing would produce a test that asserts on a tree that does not exist.
#![allow(clippy::expect_used, clippy::panic)]
// Not every helper here is used by every test binary that includes this module, and a dead-code
// warning from a shared test helper is noise that hides a real one.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::ThreadId;

use peek_cli::args;
use peek_cli::progress::{Progress, Silent};
use peek_cli::{Failure, Invocation, Output};

// Imported under a different name: this module's own `run` takes a repository and a command line,
// and inside its own body the two would otherwise be the same identifier.
use peek_cli::run as run_invocation;

/// The sources every fixture tree contains.
///
/// Small enough to be readable in a failure message and large enough to produce a graph with a
/// call edge, a type with a method, and an ambiguous name.
const LEDGER: &str = r#"//! A ledger that settles charges.

/// A charge that has not been settled.
pub struct Charge {
    /// How much.
    pub amount: u64,
}

/// What a wallet holds.
pub struct Wallet {
    /// How much it holds.
    pub amount: u64,
}

/// Sums two balances.
///
/// `amount` is declared **twice in this file**, on two unrelated types, and it is read
/// through a parameter rather than through `self`. That is the shape a name lookup
/// cannot narrow by scope: neither occurrence is inside the declaration that declares
/// the other, so the engine has two equally supported candidates and says so. The two
/// bodies above are the other shape — each reads the parameter its own function
/// declares — and they are placed, which is why this function is here as well.
pub fn total(charge: &Charge, wallet: &Wallet) -> u64 {
    charge.amount + wallet.amount
}

/// What a charge becomes once it settles.
pub struct Receipt {
    /// The charge this receipt is for.
    pub charged: u64,
}

/// Settle a charge against the ledger.
///
/// This is the one declaration in the fixture whose qualified name is unique, which is what makes
/// it addressable by name.
pub fn wallet_charge(amount: u64) -> Receipt {
    Receipt {
        charged: ledger_commit(amount),
    }
}

/// Commit an amount to the ledger.
pub fn ledger_commit(amount: u64) -> u64 {
    amount
}
"#;

const WALLET: &str = r#"//! The wallet that issues charges.

/// Issue a charge for an amount.
pub fn issue(amount: u64) -> u64 {
    amount
}
"#;

const SHARED: &str = r#"//! A second declaration of the same name, so ambiguity is reachable.

/// Render something. The same bare name as the other `render` in this fixture.
pub fn render(value: u64) -> u64 {
    value
}
"#;

const SHARED_OTHER: &str = r#"//! The other `render`, in a different file.

/// Render something else. Deliberately shares a bare name with the other `render`.
pub fn render(value: u64) -> u64 {
    value
}
"#;

/// One test's repository and the index directory it is allowed to use.
pub struct Repository {
    root: PathBuf,
    /// The same directory, spelled the way a person would write it. See [`Repository::spelled`].
    spelled: PathBuf,
    index_root: PathBuf,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Who is holding the process-global index root, and how deep they are.
struct Held {
    /// The thread that installed it. The override is process-wide, so one thread at a time.
    owner: ThreadId,
    /// How many guards that thread holds.
    depth: usize,
    /// The override each level found, outermost first, so a nested run puts back the one it
    /// found rather than clearing the override the run that called it is relying on.
    stack: Vec<PathBuf>,
}

/// The process-wide lock around [`peek_core::store::paths::set_root_override`].
///
/// **Reentrant, and that is the whole point of it being here rather than a `std::sync::Mutex`.**
/// The lock guards a *process-global*, so a second thread has to wait; but the thread that
/// already holds it must be able to take it again, because a test that holds a `Repository` and
/// then asks the harness to run a command against it is an ordinary thing to write and a
/// non-reentrant mutex turns it into a hang with no output at all — the failure mode that reads
/// as "the run produced no results" rather than as a defect. Depth is counted and the lock is
/// released at the outermost drop.
#[derive(Default)]
struct Reentrant {
    held: Mutex<Option<Held>>,
    released: Condvar,
}

impl Reentrant {
    /// Take the lock and point the engine at `root` until the returned guard is dropped.
    fn override_root(&'static self, root: &Path) -> Guard {
        let owner = std::thread::current().id();
        let mut slot = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            let mine = matches!(slot.as_ref(), Some(held) if held.owner == owner);
            if slot.is_none() || mine {
                if slot.is_some() {
                    let held = slot.as_mut().expect("the slot is occupied");
                    held.depth += 1;
                    held.stack.push(root.to_path_buf());
                } else {
                    *slot = Some(Held {
                        owner,
                        depth: 1,
                        stack: vec![root.to_path_buf()],
                    });
                }
                peek_core::store::paths::set_root_override(Some(root.to_path_buf()));
                return Guard { lock: self };
            }
            slot = self
                .released
                .wait(slot)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Hand the override back one level, and release the lock at the outermost one.
    fn release(&self) {
        let mut slot = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(held) = slot.as_mut() else {
            return;
        };
        held.depth = held.depth.saturating_sub(1);
        let _ = held.stack.pop();
        let inner = held.stack.last().cloned();
        if held.depth == 0 {
            *slot = None;
            peek_core::store::paths::set_root_override(None);
            self.released.notify_all();
        } else {
            peek_core::store::paths::set_root_override(inner);
        }
    }
}

/// The process-wide lock over the index root override.
fn override_lock() -> &'static Reentrant {
    static LOCK: OnceLock<Reentrant> = OnceLock::new();
    LOCK.get_or_init(Reentrant::default)
}

/// Keeps one repository's index root installed for as long as it is held.
pub struct Guard {
    lock: &'static Reentrant,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.lock.release();
    }
}

/// Run `body` with the engine resolving indexes under `repository`'s own index directory.
///
/// The primitive the rest of this module is built on. [`run`], [`run_verbatim`] and [`run_direct`]
/// each hold a guard for the length of one command; a test that needs the guard around something
/// else — an invocation it parsed once and will use twice, or a sink it reads after the call —
/// asks for it directly.
///
/// **The install is checked rather than assumed.** The override is process-global state the engine
/// reads on every resolution, and both ways it can be wrong make the command answer about a tree
/// nobody asked about: a run that resolved the developer's own cache reads an index this fixture
/// never wrote, and a run that resolved nothing at all refuses with `no_index`, which reads as a
/// missing index rather than as a misdirected one. Asserting here means a disagreement is
/// reported against the helper that caused it rather than against whichever test happened to be
/// asserting on a refusal.
pub fn with_index_root<T>(repository: &Repository, body: impl FnOnce() -> T) -> T {
    let _guard = override_lock().override_root(&repository.index_root);
    let resolved = peek_core::store::paths::root().expect("the index root resolves");
    assert_eq!(
        resolved,
        repository.index_root,
        "the fixture installed {} and the engine resolved {} for this run, so the two disagree \
         about where this repository's index lives",
        repository.index_root.display(),
        resolved.display(),
    );
    body()
}

impl Repository {
    /// A fixture with the standard sources.
    #[must_use]
    pub fn small(label: &str) -> Self {
        let repository = Self::bare(label);
        repository.write("src/ledger.rs", LEDGER);
        repository.write("src/wallet.rs", WALLET);
        repository.write("src/ui/a.rs", SHARED);
        repository.write("src/ui/b.rs", SHARED_OTHER);
        repository
    }

    /// A fixture with no source files at all.
    ///
    /// **Not a cold build.** "Cold" is about the index — no generation has been committed — and a
    /// repository with files in it is cold on its first run too. The two are separate facts and
    /// conflating them is how a test ends up asserting that files were indexed in a directory with
    /// no files in it. See `a_cold_build_reports_the_mode_and_the_files_it_indexed` for the
    /// assertion that was impossible and why the fixture changed rather than the assertion.
    #[must_use]
    pub fn empty(label: &str) -> Self {
        Self::bare(label)
    }

    /// A fixture with only its directories.
    fn bare(label: &str) -> Self {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("peek-cli-{label}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo")).expect("create the repository");
        std::fs::create_dir_all(base.join("cache")).expect("create the index root");
        let root = base
            .join("repo")
            .canonicalize()
            .expect("canonicalise the repository");
        let index_root = base
            .join("cache")
            .canonicalize()
            .expect("canonicalise the index root");
        let spelled = without_the_verbatim_prefix(&root);
        Self {
            root,
            spelled,
            index_root,
        }
    }

    /// The repository root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repository root without the extended-length prefix `canonicalize` puts on it.
    ///
    /// **A second spelling of one directory, and it exists because the first cannot carry a
    /// climb.** On Windows `canonicalize` answers `\\?\C:\repo`, and a verbatim spelling is handed
    /// to the filesystem exactly as written — `std` returns one straight out of its own
    /// path-length handling with no normalisation pass over it — so nothing ever takes a `..` off
    /// it. A climb written after the canonical spelling therefore names nothing at all, and both
    /// the harness and the command it runs are right to say the filesystem could not answer. This
    /// is the spelling a person would have written, and the filesystem reads it the way it was
    /// written.
    ///
    /// **The two are the same directory and it is the canonical one the engine answers with.**
    /// Naming a root this way is not a different repository: the two canonicalise to one location,
    /// so the identity, the index and every comparison against [`Self::root`] agree.
    ///
    /// On Unix there is no prefix and this is [`Self::root`], so a test that builds its spellings
    /// from it is building them from the same string on every platform.
    #[must_use]
    pub fn spelled(&self) -> &Path {
        &self.spelled
    }

    /// [`Self::spelled`] as a string, for a command line.
    #[must_use]
    pub fn spelled_str(&self) -> &str {
        self.spelled.to_str().expect("a temporary path is UTF-8")
    }

    /// The repository root as a string, for a command line.
    #[must_use]
    pub fn root_str(&self) -> &str {
        self.root.to_str().expect("a temporary path is UTF-8")
    }

    /// The repository's own identity, which the engine derives from the canonical root and the
    /// git common directory.
    ///
    /// **It is a hash of where the fixture was created, so it is a different value on every run
    /// and in every other fixture.** That is what makes it worth naming: a command that prints it
    /// prints a cache key, which is useful to somebody holding two checkouts and useless in a
    /// committed file, so a test comparing renderings has to substitute it. `index_path` is built
    /// from this rather than deriving it again, so the directory a test looks in and the directory
    /// a command writes cannot disagree.
    #[must_use]
    pub fn identity(&self) -> String {
        peek_core::store::RepoId::discover(&self.root)
            .expect("derive the identity")
            .as_str()
            .to_owned()
    }

    /// Where this repository's index lives, which the engine derives from its identity.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.index_root.join(self.identity()).join("index.db")
    }

    /// A path outside this repository but inside its temporary base, for the containment tests.
    #[must_use]
    pub fn sibling(&self, name: &str) -> PathBuf {
        self.index_root.join(name)
    }

    /// Write a file, creating its directory.
    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the parent directory");
        }
        std::fs::write(&path, contents).expect("write the file");
    }

    /// Delete a file.
    pub fn remove(&self, relative: &str) {
        let path = self.root.join(relative);
        let _ = std::fs::remove_file(&path);
    }

    /// Write a file that is not valid UTF-8, for the decode-refusal path.
    ///
    /// The bytes are a lone `0xFF` followed by a NUL, which no UTF-8 sequence can start, so this is
    /// a file that *cannot* be decoded rather than one that merely looks odd.
    pub fn write_invalid_utf8(&self, relative: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the parent directory");
        }
        std::fs::write(&path, [0xff, 0x41]).expect("write the file");
    }

    /// The index root, for a test that needs to look inside it.
    #[must_use]
    pub fn index_root(&self) -> &Path {
        &self.index_root
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        // No override to restore: one is installed only while a `Guard` is held, and a guard is
        // held only for the length of a run. That is why dropping a repository cannot clear a
        // *different* repository's index root, which it used to be able to do.
        let base = self
            .root
            .parent()
            .map_or_else(|| self.index_root.clone(), Path::to_path_buf);
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// `canonicalize`'s answer with the extended-length prefix taken off it.
///
/// The same operation `peek_cli::paths` does for its own containment tests, for the same reason:
/// a prefix difference carries no location, so it is the one difference two spellings of one
/// directory may be built to have — and the one that makes a climb written after them resolvable.
///
/// **`\\?\UNC\server\share` is a separate case rather than four bytes off the front.** A share's
/// verbatim spelling has the prefix in the *middle* of it, so taking it off is not the same
/// operation there as it is on a drive, and doing it the drive way would spell a directory called
/// `UNC`.
///
/// A path that is not UTF-8 keeps its prefix. Substituting replacement characters would hand back a
/// *different* directory, and a spelling that names the wrong place is worse than a spelling the
/// filesystem will not read.
fn without_the_verbatim_prefix(root: &Path) -> PathBuf {
    // Unix has no prefix component, so there is nothing to take off and `canonicalize`'s answer is
    // the only spelling there is — a pair a test builds from it is the path against itself, which
    // is the state this function exists to end.
    if !cfg!(windows) {
        return root.to_path_buf();
    }
    let Some(verbatim) = root.to_str() else {
        return root.to_path_buf();
    };
    if let Some(share) = verbatim.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{share}"));
    }
    if let Some(bare) = verbatim.strip_prefix(r"\\?\") {
        return PathBuf::from(bare);
    }
    // Nothing to take off: a path with no prefix component, which the canonical spelling is the
    // only spelling of.
    root.to_path_buf()
}

/// What a run produced: the output, and the progress narration the sink collected.
#[derive(Debug, Clone)]
pub struct Ran {
    /// The output, whatever the status.
    pub output: Output,
    /// The narration the sink received. Empty under `--quiet`.
    pub narration: Vec<String>,
}

/// How a command line has to be told which repository it is about.
///
/// Read out of [`args::COMMANDS`] rather than written here, because the table is the authority
/// and a second list of which command takes what is a list that can disagree with the parser.
/// Getting this wrong is worse than the defect it fixes: a `--root` handed to `index` is accepted
/// by the parser and **ignored**, so a fixture that used the flag for every command would index
/// the package directory and report a confident count for the wrong tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Naming {
    /// The command's own `[PATH]` positional, which only `index` and `watch` have.
    Positional,
    /// The global `--root` flag, which is every other command that reads a repository.
    Flag,
    /// The command reads no repository: `help` and `version`. Naming one would be a lie about
    /// what the command read, and the parser ignores it, so it would also be a dead flag.
    Neither,
}

/// The shape the command in `argv` takes its repository in.
fn naming_for(command: &args::Command) -> Naming {
    if matches!(command, args::Command::Help | args::Command::Version) {
        return Naming::Neither;
    }
    let spec = args::COMMANDS
        .iter()
        .find(|spec| spec.name == command.name())
        .expect("every command `parse` produces is in the table that produced it");
    match spec.root {
        args::RootSource::Positional => Naming::Positional,
        args::RootSource::Flag => Naming::Flag,
    }
}

/// Whether `--root` appears as a flag, rather than as a value after a `--` terminator.
///
/// The terminator is honoured because `peek explain -- --root` asks about something *named*
/// `--root`, and counting that as the flag would leave the real command unpointed.
fn names_a_root_flag(argv: &[std::ffi::OsString]) -> bool {
    let mut after_terminator = false;
    for token in argv {
        let text = token.to_string_lossy();
        if after_terminator {
            continue;
        }
        if text == "--" {
            after_terminator = true;
        } else if text == "--root" || text.starts_with("--root=") {
            return true;
        }
    }
    false
}

/// Whether the line already names a repository, so that nothing is added to it.
///
/// **Already-named is left alone in every shape, not only where injection would collide.** A test
/// that spells `--root <root>/src/..` on purpose is exercising path canonicalisation, and adding a
/// second `--root` would be a `RepeatedFlag` usage error rather than the answer it is testing for.
fn names_a_root(argv: &[std::ffi::OsString], command: &args::Command) -> bool {
    match naming_for(command) {
        Naming::Neither => true,
        Naming::Flag => names_a_root_flag(argv),
        // The parser has already decided which token was the path, so it is asked rather than
        // counted here. `.` is the default, and a line that left it alone named nothing.
        Naming::Positional => command
            .root()
            .is_some_and(|root| root != &PathBuf::from(".")),
    }
}

/// The command line a run will actually use, with the repository named if the test did not.
///
/// A line the parser refuses is returned untouched. That is not a special case for its own sake:
/// those tests exist to assert on the refusal, and a `--root` appended to `--nonsense` could turn
/// an `unknown_flag` into a `wrong_arity` and the test would still pass while checking nothing.
fn command_line(argv: &[&str], root: &Path) -> Vec<std::ffi::OsString> {
    let mut tokens: Vec<std::ffi::OsString> =
        argv.iter().copied().map(std::ffi::OsString::from).collect();
    let Ok(parsed) = args::parse(tokens.clone()) else {
        return tokens;
    };
    if names_a_root(&tokens, &parsed.command) {
        return tokens;
    }
    match naming_for(&parsed.command) {
        // Appended rather than inserted after the command name. The parser splits flags from
        // positionals in one pass over the whole line, so the position is free, and appending
        // cannot land in the middle of a flag's value.
        Naming::Positional => tokens.push(std::ffi::OsString::from(root)),
        Naming::Flag => {
            tokens.push(std::ffi::OsString::from("--root"));
            tokens.push(std::ffi::OsString::from(root));
        }
        Naming::Neither => {}
    }
    tokens
}

/// The directory `path` names, or why the filesystem would not say.
///
/// **The whole spelling is asked about first, because that is the authority and a kernel has
/// nothing to be taught about `..`.** Where that answer comes back there is nothing left to
/// reconcile, and where it does not the answer is not "somewhere else" — it is that the question
/// was not answerable, and the only thing that can change that is the shape of the spelling.
///
/// **A trailing `.` or `..` is arithmetic on the name in front of it rather than a name of its
/// own**, so the part in front is asked about instead and the arithmetic is spent on whatever comes
/// back. That is the whole case on Windows: a verbatim (`\\?\`) spelling carries no `..` for the
/// path parser to remove before the filesystem is asked, so `<root>/src/..` is a spelling the
/// filesystem will only answer about once the climb has been taken off the front of it. One
/// directory, two spellings, and the second one is not a different directory.
///
/// **The climb is spent on the *resolved* location, and that is what keeps this away from being
/// the one-line fix.** Normalising the spelling lexically and comparing strings would answer
/// `<root>` for `<root>/out/..`, where `out` is a link leaving the tree — so the harness would pass
/// a command pointed at a directory that is not the fixture, which is the one failure this
/// assertion exists to catch, and a fix that makes the check pass by comparing less is worse than
/// the defect. So the head is *asked about* rather than assumed: a climb over a name that does not
/// exist resolves nothing, and a climb over a link is spent on the link's target.
///
/// **A trailing `..` is all this can spend, and a `Path` is why.** `Path` does not expose a `.`
/// that is not the first component, so a spelling with one hidden in the middle of itself is
/// reported as unanswered. That is a refusal rather than a wrong answer, which is the right way
/// round for a check whose job is to notice.
///
/// **The peel reads the spelling rather than `Path::components`, because for one spelling the two
/// disagree and the spelling is the one that is right.** `std` splits a verbatim (`\\?\`) path on
/// `\` alone — it assumes such a path is already normalised, and has nothing to normalise it with —
/// so in `\\?\C:\repo/src/..` the tail `repo/src/..` arrives as **one name**, and the climb at the
/// end of it is not a component at all. `components().next_back()` answers `Normal`, the peel stops
/// on a spelling that plainly ends in a climb, and the harness reports that it cannot tell what the
/// directory is. [`peel`] asks the question of the spelling itself, where the answer is visible.
fn location_of(path: &Path) -> Result<PathBuf, String> {
    let unanswered = match path.canonicalize() {
        Ok(location) => return Ok(location),
        Err(error) => error,
    };
    let mut head = path.to_path_buf();
    // What each step spent: `true` for a climb, `false` for a `.`, which moves nowhere. Held as a
    // flag rather than as a component because a `Component` borrows the head it was read off, and
    // the head is reassigned on every turn.
    let mut spent: Vec<bool> = Vec::new();
    // `while let` rather than `loop { let Some(..) else { break } }`: the loop's whole
    // continuation condition *is* the peel succeeding, so saying it twice invites the two to drift.
    while let Some((climbs, parent)) = peel(&head) {
        spent.push(climbs);
        head = parent;
        // Asked about, not assumed: a head that will not resolve leaves the spelling unanswered
        // rather than handing back a location built out of names nothing has heard of.
        let Ok(location) = head.canonicalize() else {
            continue;
        };
        let mut answer = location;
        for step in spent.iter().rev() {
            if *step {
                answer.pop();
            }
        }
        return Ok(answer);
    }
    Err(format!(
        "the filesystem will not resolve it as a whole ({unanswered}), and it does not end in a \
         climb that can be taken off the front of it"
    ))
}

/// `path` with one trailing `.` or `..` taken off the front of it, and what taking it cost.
///
/// **`None` means the spelling does not end in arithmetic**, which is the common case: a trailing
/// name is a name. The caller answers with what the filesystem said and stops, so a spelling that
/// ends in an ordinary name is one the filesystem was supposed to have resolved on its own.
///
/// **A spelling that is not UTF-8 is a spelling this cannot take a climb off, and that is a
/// refusal.** The cut is on a character boundary because a `&str` has to be one, and a path this
/// cannot cut is a path it will not guess at. Both sides of the comparison are the fixture's own
/// directory, which the fixture already requires to be UTF-8 to put on a command line at all, so
/// this costs nothing the harness could otherwise have answered.
fn peel(path: &Path) -> Option<(bool, PathBuf)> {
    let text = path.to_str()?;
    // Separators on the end are not part of the name that follows them, so they are set aside
    // before the last name is read and kept on the head rather than lost with it.
    let trimmed = text.trim_end_matches(is_name_separator);
    let cut = trimmed.rfind(is_name_separator).map_or(0, |at| at + 1);
    let (head, name) = trimmed.split_at(cut);
    let climbs = match name {
        ".." => true,
        "." => false,
        _ => return None,
    };
    let head = platform_separators(head.trim_end_matches(is_name_separator));
    Some((climbs, PathBuf::from(head)))
}

/// `text` with every `/` written as the platform's own separator.
///
/// **Windows only, and it is a robustness measure rather than a tidy-up.** `/` cannot be part of a
/// name on Windows, so a `/` in a spelling separates two names and nothing else, and the two
/// spellings below name the same directory. A verbatim path reaches the filesystem exactly as
/// written, though, so whether that filesystem reads a `/` inside one at all is a question this
/// need not depend on: writing the separator the platform uses takes the question away, and a head
/// the filesystem is certainly going to read is worth more than the two bytes saved.
///
/// On Unix a backslash is an ordinary character in a name and this does nothing at all, which is
/// what keeps the peel there byte-for-byte what `Path::parent` would have returned.
fn platform_separators(text: &str) -> String {
    if cfg!(windows) {
        text.replace('/', std::path::MAIN_SEPARATOR_STR)
    } else {
        text.to_owned()
    }
}

/// Whether `character` ends one name and begins the next.
///
/// **`/` on Unix, where a backslash is an ordinary character in a name**, and both separators on
/// Windows, which is what the filesystem's own path parser does with a path it is free to
/// normalise. The verbatim spelling is the case that has to be reasoned about rather than read
/// off: there `std` will not treat `/` as a separator, and this has to anyway — not as a guess
/// about what was meant, but because a Windows name may not contain `/` at all. That is why a
/// verbatim spelling carrying one is refused with "the filename, directory name, or volume label
/// syntax is incorrect" rather than reported as a missing file: it cannot be a name, so the only
/// reading left is that `/` separates one.
fn is_name_separator(character: char) -> bool {
    character == '/' || (cfg!(windows) && character == '\\')
}

/// Fail a run that is not pointed at `repository`.
///
/// **The assertion the whole injection exists to make possible.** `--root` defaults to `.`, the
/// working directory of a test binary is the package directory, and a command pointed at
/// `crates/peek-cli` answers `no_index` — which reads as a wrong answer type rather than as the
/// missing flag it is. Checking it here means the next command shape, or the next test, cannot
/// reintroduce that quietly: it would fail in the harness with the line that caused it.
///
/// **Both sides are located by the filesystem, and both through one function**, because a spelling
/// and a location are not comparable things: `canonicalize` answers in the extended-length form on
/// Windows and expands a short name on the way to a directory, so a directory the filesystem named
/// and a root the caller spelled are two strings and one place. That is what [`location_of`] is
/// for, and the pair it replaces — the fixture's root canonicalised against the named root
/// *uncanonicalised when the filesystem declined* — held one side that could not answer and one
/// side that could, so a test spelling `<root>/src/..` compared its own spelling with a location
/// and reported a harness failure for a command that was pointed at the fixture.
///
/// **"The filesystem would not say" is now its own outcome, and it is reported as that.** It used
/// to be reported as "this command was not pointed at the fixture", which is a claim about a cause
/// the harness had not established: all it had observed was that it could not look. Two roots can
/// reach that state — a path that names nothing at all, and a spelling the filesystem will not
/// answer as a whole — and the message named the first while the check could not tell them apart.
fn assert_named_root(named: Option<&PathBuf>, repository: &Repository, argv: &[&str]) {
    let Some(named) = named else {
        return;
    };
    let expected = location_of(repository.root()).expect("the fixture's root resolves");
    let actual = location_of(named).unwrap_or_else(|reason| {
        panic!(
            "the harness cannot tell which directory {named:?} names — {reason} — so it cannot \
             check that this command pointed at the fixture, and it will not guess at one. A root \
             the filesystem will not resolve is a fact about the filesystem, not about the command \
             line. {argv:?}"
        )
    });
    assert_eq!(
        actual, expected,
        "the fixture did not point this command at its own repository, so it answered about \
         {named:?} — which is not a fixture. A new command shape, or a command line that names \
         a root the harness did not recognise, is the likely cause. {argv:?}"
    );
}

/// Run a command line against `repository`, naming the repository if the line does not.
///
/// See [`command_line`] for how the shape is chosen, and [`assert_named_root`] for the check that
/// makes the choice load-bearing rather than hopeful.
///
/// A usage error becomes an [`Output`] with `Status::Usage` rather than a panic, so a test can
/// assert on the exit code of a bad command line without a separate code path. A refusal comes back
/// from the library as an `Err(Failure)` and is turned into an `Output` the same way, which is what
/// the binary's entry point does.
///
/// **Both of those go through `peek_cli` rather than being built here.** The two representations
/// meeting is a product decision — it decides what the *answer* field says for a command that did
/// not answer — so the meeting happens in one place in the library. A fixture that assembled its
/// own `Output` is a second answer to that question, and it is where a failure used to come back
/// carrying the usage text.
pub fn run(repository: &Repository, argv: &[&str]) -> Ran {
    let tokens = command_line(argv, repository.root());
    execute(repository, tokens, Some(argv))
}

/// Run a command line **exactly as written**, naming no repository at all.
///
/// For the tests whose claim is about the path or the root rather than about a command's answer.
/// **The exemption is a separate function on purpose.** Listing the exempt tests inside [`run`] and
/// matching on their names would keep the code in one place, but a test whose meaning changed
/// would then be invisible at the call site, and an invisible change of meaning is the one failure
/// mode this whole change is about. Written this way, the test that opts out says so, and the
/// reader can see which tests are about the argument and which are about the command.
///
/// The index root is still the repository's own, so the command cannot reach a developer's cache.
pub fn run_verbatim(repository: &Repository, argv: &[&str]) -> Ran {
    let tokens: Vec<std::ffi::OsString> =
        argv.iter().copied().map(std::ffi::OsString::from).collect();
    execute(repository, tokens, None)
}

/// Run a command line **exactly as written**, driving `peek_cli::run` with a sink the test owns.
///
/// This is [`peek_cli::run`] with the one thing it cannot do for itself: it resolves the index
/// root from the process-global override while the guard is held. A test needs it to read the raw
/// result — a [`Failure`] rather than the unified [`Ran`] — or to watch what its own sink was told,
/// neither of which [`run`] can arrange.
///
/// **It exists because parse, scope and run were three separate steps at every call site, and a
/// call site can spell two of them.** Two tests in `golden.rs` built the invocation, handed it to
/// `peek_cli::run`, and forgot the scoping: the `index` command in the same test wrote the index
/// under this fixture's cache while the `context` command that followed resolved it from the
/// process default, found nothing, and refused with `no_index`. Both tests went on to report a
/// budget problem, because a refusal is a refusal and only the *kind* disagreed. Here the three
/// steps are one call.
///
/// The line is parsed verbatim and gets no repository named for it, so a caller must say where it
/// means. That is the trade: this helper does not know a command's shape the way [`run`] does, and
/// pretending otherwise is how `--root` came to be handed to `index`, which accepts and ignores it.
///
/// The guard is released before this returns. The result is owned data and the engine resolves
/// nothing after the call, so holding the override longer would narrow the window in which other
/// tests are blocked for no gain.
pub fn run_direct(
    repository: &Repository,
    argv: &[&str],
    progress: &mut dyn Progress,
) -> Result<Output, Failure> {
    let tokens: Vec<std::ffi::OsString> =
        argv.iter().copied().map(std::ffi::OsString::from).collect();
    let invocation = args::parse(tokens).expect("the command line a test wrote must parse");
    with_index_root(repository, || run_invocation(&invocation, progress))
}

/// Parse, check and run one command line. `written` is the command line as the test typed it, and
/// is `Some` only when the caller asked for the repository to be named and checked.
fn execute(
    repository: &Repository,
    tokens: Vec<std::ffi::OsString>,
    written: Option<&[&str]>,
) -> Ran {
    let invocation: Invocation = match args::parse(tokens.clone()) {
        Ok(invocation) => invocation,
        Err(error) => {
            return Ran {
                output: peek_cli::usage_failed(&args::named_command(&tokens), &error),
                narration: Vec::new(),
            };
        }
    };
    if let Some(argv) = written {
        assert_named_root(invocation.command.root(), repository, argv);
    }
    // The library resolves the index root from the process-global override, which is installed
    // here for the length of the run and nowhere else. The sink is all a test needs to supply.
    with_index_root(repository, || {
        let mut silent = Silent;
        match run_invocation(&invocation, &mut silent) {
            Ok(output) => {
                let narration = output.progress.clone();
                Ran { output, narration }
            }
            Err(failure) => Ran {
                output: peek_cli::declined(&failure),
                narration: Vec::new(),
            },
        }
    })
}

/// Removes a path when dropped, so a failing test does not leave state behind.
pub struct Cleanup(pub PathBuf);

impl Cleanup {
    /// Clean up this path.
    #[must_use]
    pub fn on(path: PathBuf) -> Self {
        Self(path)
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
