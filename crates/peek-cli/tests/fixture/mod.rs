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
//! consequence is that the tests in this binary cannot run concurrently against different roots, so
//! the lock below serialises them. That is a real cost and it is paid on purpose: the alternative
//! is a test that reaches the developer's own index, which is a far worse surprise.
//!
//! # The fixture's own surface
//!
//! A small, fixed Rust tree with one file per language feature that produces a relation the query
//! surface needs: a function that calls another, a type with a method, a bare name, and a name that
//! two files both declare so that ambiguity is reachable without inventing an index by hand.

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
use std::sync::{Mutex, MutexGuard, OnceLock};

use peek_cli::args;
use peek_cli::exit::kind;
use peek_cli::progress::Silent;
use peek_cli::{Command, Invocation, Output};

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

/// One test's repository, its index directory, and the lock that serialises the override.
pub struct Repository {
    root: PathBuf,
    /// Held for the repository's lifetime. Dropping it releases the override lock, so the
    /// repository's index directory stops being the process-wide root before the next test sets
    /// its own.
    _guard: MutexGuard<'static, ()>,
    index_root: PathBuf,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The process-wide lock guarding [`peek_core::store::paths::set_root_override`].
fn override_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let mutex = LOCK.get_or_init(|| Mutex::new(()));
    // A poisoned lock is safe to take: the guard is dropped on every path out of `run_with`, so
    // the override is always restored even when a test panics.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    /// A fixture with no source files at all, for the cold and empty cases.
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

        let guard = override_lock();
        peek_core::store::paths::set_root_override(Some(index_root.clone()));
        Self {
            root,
            _guard: guard,
            index_root,
        }
    }

    /// The repository root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repository root as a string, for a command line.
    #[must_use]
    pub fn root_str(&self) -> &str {
        self.root.to_str().expect("a temporary path is UTF-8")
    }

    /// Where this repository's index lives, which the engine derives from its identity.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        let repo = peek_core::store::RepoId::discover(&self.root).expect("derive the identity");
        self.index_root.join(repo.as_str()).join("index.db")
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
        // Restore the platform default *before* the directory is removed, so a later test cannot
        // resolve a path under a directory that no longer exists. The lock guard drops after this
        // body, so the ordering is: restore, then release.
        peek_core::store::paths::set_root_override(None);
        let base = self
            .root
            .parent()
            .map_or_else(|| self.index_root.clone(), Path::to_path_buf);
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// What a run produced: the output, and the progress narration the sink collected.
#[derive(Debug, Clone)]
pub struct Ran {
    /// The output, whatever the status.
    pub output: Output,
    /// The narration the sink received. Empty under `--quiet`.
    pub narration: Vec<String>,
}

/// Run a command line against `repository`.
///
/// A usage error becomes a [`Output`] with `Status::Usage` rather than a panic, so a test can
/// assert on the exit code of a bad command line without a separate code path. This is the only
/// place the two representations are unified, and it is the boundary the binary's `main` mirrors.
pub fn run(_repository: &Repository, argv: &[&str]) -> Ran {
    let owned: Vec<std::ffi::OsString> = argv
        .iter()
        .map(|argument| std::ffi::OsString::from(*argument))
        .collect();
    let invocation: Invocation = match args::parse(owned) {
        Ok(invocation) => invocation,
        Err(error) => {
            return Ran {
                output: usage_output(argv, error),
                narration: Vec::new(),
            };
        }
    };
    // The library resolves the index root from the process-global override this repository set, so
    // the sink is all a test needs to supply.
    let mut silent = Silent;
    match run_invocation(&invocation, &mut silent) {
        Ok(output) => {
            let narration = output.progress.clone();
            Ran { output, narration }
        },
        Err(failure) => Ran {
            output: failure_output(&invocation.command, &failure),
            narration: Vec::new(),
        },
    }
}

/// A [`Output`] carrying a usage failure, so a test can assert on its exit code.
///
/// The refusal kind is derived from the error variant rather than hardcoded, because "every
/// argument error exits 2" is only a useful claim if the reason is carried as well.
fn usage_output(argv: &[&str], error: args::UsageError) -> Output {
    let refusal_kind = match &error {
        args::UsageError::UnknownFlag { .. } => kind::UNKNOWN_FLAG,
        args::UsageError::UnknownCommand { .. } => kind::UNKNOWN_COMMAND,
        args::UsageError::RepeatedFlag { .. }
        | args::UsageError::UnexpectedValue { .. }
        | args::UsageError::FlagNotValidHere { .. } => kind::BAD_FLAG_USE,
        args::UsageError::MissingValue { .. } => kind::MISSING_VALUE,
        args::UsageError::NotANumber { .. } => kind::NOT_A_NUMBER,
        args::UsageError::MissingArgument { .. } | args::UsageError::TooManyArguments { .. } => {
            kind::WRONG_ARITY
        }
        args::UsageError::NotUtf8 { .. } => kind::NOT_UTF8,
    };
    let command = argv
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .copied()
        .unwrap_or("peek");
    Output {
        version: peek_core::VERSION.to_owned(),
        command: command.to_owned(),
        status: peek_cli::Status::Usage,
        exit_code: peek_cli::exit::EXIT_USAGE,
        root: String::new(),
        index_path: String::new(),
        index_existed: false,
        answer: peek_cli::Answer::Help(peek_cli::answer::HelpAnswer {
            usage: args::help_text(),
        }),
        refusal: Some(peek_cli::Refusal::new(refusal_kind, error.message())),
        progress: Vec::new(),
    }
}

/// An [`Output`] carrying a failure, so a test can assert on its exit code the same way.
fn failure_output(command: &Command, failure: &peek_cli::Failure) -> Output {
    Output {
        version: peek_core::VERSION.to_owned(),
        command: command.name().to_owned(),
        status: failure.status,
        exit_code: failure.exit_code(),
        root: String::new(),
        index_path: String::new(),
        index_existed: false,
        answer: peek_cli::Answer::Help(peek_cli::answer::HelpAnswer {
            usage: args::help_text(),
        }),
        refusal: Some(failure.refusal.clone()),
        progress: Vec::new(),
    }
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
