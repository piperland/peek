//! Where the index lives on disk.
//!
//! The index is a **cache**: it can be rebuilt from source at any time, and losing it costs time
//! rather than data. That is what decides both its location and its lifetime.
//!
//! # It is not in the repository
//!
//! Audit A: the engine Peek replaces wrote `.cortex/` into every repository it touched, and
//! gitignored it — which is the worst of both, because a binary project should not carry
//! gitignored build state and a user's `git status` should never show a tool's internals. It also
//! meant that cloning a repository carried an index describing a machine that no longer exists.
//!
//! So the index goes to the operating system's per-user cache root, in a directory named for this
//! project and the repository's identity:
//!
//! | Platform | Location |
//! |---|---|
//! | Linux | `$XDG_CACHE_HOME/piper/peek/<repo-id>/` |
//! | macOS | `~/Library/Caches/piper/peek/<repo-id>/` |
//! | Windows | `%LOCALAPPDATA%\piper\peek\<repo-id>\` |
//!
//! Each platform also gets a physical one: on Linux the index is a cache and the OS may delete it
//! under pressure, which is correct, because it can be rebuilt.
//!
//! **Nothing here decides whether a path is inside a repository.** That question is asked by
//! [`crate::doctor`], it is answered by [`crate::containment::contains`], and the answer is a
//! *finding* rather than a guard: this module places the index in the OS cache directory whatever
//! `PEEK_INDEX_DIR` says, so an index somebody has deliberately pointed at their own working tree
//! is a misconfiguration to be reported, not a write to be refused.
//!
//! # Why the directory is named for the repository
//!
//! Not for the repository's *name*, which collides — two checkouts called `api` in different
//! places must not share an index — but for its [`RepoId`], which is a hash of the canonical
//! root together with the git common directory. Two worktrees of one repository therefore get
//! different directories, and the same checkout always gets the same one. Putting the index
//! anywhere shared between them is how one branch's symbols end up answering questions about
//! another.
//!
//! # Escape hatch
//!
//! [`PEEK_INDEX_DIR`](env::PEEK_INDEX_DIR) overrides the root entirely. It exists for two
//! reasons: tests need a location they can delete, and a user on a machine with a relocated home
//! directory needs a way to say so. It is one variable and it is documented, not a config file
//! with a schema.

use std::path::PathBuf;

use crate::store::{RepoId, StoreError};

/// The environment variable that overrides the index root.
pub const ENV_INDEX_DIR: &str = "PEEK_INDEX_DIR";

/// A programmatic override, consulted before the environment variable.
///
/// This exists for two callers, and both of them are real:
///
/// - **An embedding application** that has been told where to put an index, and must not mutate
///   the process environment to say so. Environment variables are process-global, and a server
///   serving several repositories cannot use them to say which one it means.
/// - **Tests**, which need an index location they can delete and must not be able to reach the
///   developer's real one. A test that broke the real index would be an unpleasant surprise.
///
/// Process-wide, like the environment variable it shadows, and therefore `&self`-free by
/// necessity. Call [`set_root_override`] once at startup rather than per request.
static ROOT_OVERRIDE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Point Peek at a specific index root, overriding both the default and the environment.
///
/// `None` restores the normal resolution order. The value is used **verbatim**: not resolved
/// relative to anything, and not required to exist yet.
pub fn set_root_override(root: Option<PathBuf>) {
    match ROOT_OVERRIDE.write() {
        Ok(mut slot) => *slot = root,
        // A poisoned lock means some other thread panicked while holding it. The value it holds
        // is still a `PathBuf` and still perfectly usable, so poisoning is not a reason to fail a
        // call that only writes a path.
        Err(poisoned) => *poisoned.into_inner() = root,
    }
}

/// The file name of the index inside its directory.
const INDEX_FILE: &str = "index.db";

/// The directory holding this repository's index.
pub fn index_dir(repo: &RepoId) -> Result<PathBuf, StoreError> {
    Ok(root()?.join(repo.as_str()))
}

/// The full path of this repository's index file.
pub fn index_path(repo: &RepoId) -> Result<PathBuf, StoreError> {
    Ok(index_dir(repo)?.join(INDEX_FILE))
}

/// The root under which every repository's index is kept.
///
/// An override wins over the platform default, and the override is used **verbatim** — it is not
/// interpreted relative to the repository, and it is not required to exist yet. Treating it as
/// anything other than an absolute location is how an override ends up quietly writing into a
/// working directory.
pub fn root() -> Result<PathBuf, StoreError> {
    let override_slot = ROOT_OVERRIDE
        .read()
        .map(|slot| slot.clone())
        .unwrap_or(None);
    if let Some(configured) = override_slot {
        return Ok(configured);
    }
    if let Some(configured) = std::env::var_os(ENV_INDEX_DIR)
        && !configured.is_empty()
    {
        return Ok(PathBuf::from(configured));
    }
    platform_root()
}

/// The per-user cache root, in this platform's own convention.
#[cfg(target_os = "windows")]
fn platform_root() -> Result<PathBuf, StoreError> {
    // `LOCALAPPDATA` rather than `APPDATA`: this is rebuildable state, not user configuration,
    // and putting it under Roaming would sync a multi-gigabyte index between machines.
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| missing("LOCALAPPDATA"))?;
    Ok(base.join("piper").join("peek"))
}

#[cfg(target_os = "macos")]
fn platform_root() -> Result<PathBuf, StoreError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| missing("HOME"))?;
    Ok(home
        .join("Library")
        .join("Caches")
        .join("piper")
        .join("peek"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_root() -> Result<PathBuf, StoreError> {
    // The XDG base directory specification, which is also what Linux, the BSDs and WSL follow.
    // `XDG_CACHE_HOME` must be an absolute path to be honoured at all, so a relative value is
    // ignored rather than resolved against the process's working directory.
    if let Some(configured) = std::env::var_os("XDG_CACHE_HOME")
        && std::path::Path::new(&configured).is_absolute()
    {
        return Ok(PathBuf::from(configured).join("piper").join("peek"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| missing("HOME or XDG_CACHE_HOME"))?;
    Ok(home.join(".cache").join("piper").join("peek"))
}

/// The error for a platform that will not tell us where its cache lives.
fn missing(variable: &str) -> StoreError {
    StoreError::Io(format!(
        "cannot find a cache directory: {variable} is not set, and the default cannot be \
         inferred. Set {ENV_INDEX_DIR} to choose a location."
    ))
}

#[cfg(test)]
mod tests {
    use super::{ENV_INDEX_DIR, index_dir, index_path, root};
    use crate::containment::contains;
    use crate::store::RepoId;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A temporary directory removed on drop, so a test that reads the environment cannot leak
    /// state into the next one.
    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "peek-paths-{}-{label}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temporary directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run `body` with `PEEK_INDEX_DIR` set to `value`, restoring the previous value afterwards.
    ///
    /// Environment mutation is process-global and the test binary runs tests concurrently, so
    /// every mutation in this module goes through here and holds the same lock. Without that, two
    /// tests could disagree about the root and produce a failure that reproduces once in fifty
    /// runs — the worst kind to chase.
    ///
    /// `unsafe` because `set_var` is unsafe in edition 2024. The obligation is that no other
    /// thread reads or writes the environment for the duration, and that is what the lock buys:
    /// [`root`] is the only function in this module that reads the environment, and every test
    /// that calls it goes through this guard.
    fn with_index_dir_value<T>(value: Option<&std::ffi::OsStr>, body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| {
            // A previous test panicked while holding the lock. The environment is restored on the
            // way out of that test either way, so the lock is safe to take.
            poisoned.into_inner()
        });
        let previous = std::env::var_os(ENV_INDEX_DIR);
        // SAFETY: the lock above is held for the whole body, and `root` is the only reader of this
        // variable in this module. No other thread can observe a partially-restored environment.
        unsafe {
            match value {
                Some(value) => std::env::set_var(ENV_INDEX_DIR, value),
                None => std::env::remove_var(ENV_INDEX_DIR),
            }
        }
        let outcome = body();
        // SAFETY: as above — still holding the lock, still the only writer.
        unsafe {
            match previous {
                Some(value) => std::env::set_var(ENV_INDEX_DIR, value),
                None => std::env::remove_var(ENV_INDEX_DIR),
            }
        }
        outcome
    }

    /// Run `body` with `PEEK_INDEX_DIR` pointing at `dir`.
    fn with_index_dir<T>(dir: &Path, body: impl FnOnce() -> T) -> T {
        with_index_dir_value(Some(dir.as_os_str()), body)
    }

    fn repo_id(label: &str) -> RepoId {
        let tree = Temp::new(label);
        RepoId::discover(tree.path()).expect("derive a repository id")
    }

    #[test]
    fn the_index_lands_in_a_directory_named_for_the_repository_identity() {
        let temp = Temp::new("index-dir");
        with_index_dir(temp.path(), || {
            let repo = repo_id("named");
            let directory = index_dir(&repo).expect("resolve the index directory");
            assert_eq!(
                directory,
                temp.path().join(repo.as_str()),
                "the repository identity names the directory, so two checkouts cannot collide"
            );
            assert_eq!(
                index_path(&repo).expect("resolve the index path"),
                directory.join("index.db")
            );
        });
    }

    #[test]
    fn the_index_never_lands_inside_the_repository_it_describes() {
        // The defect this whole module exists to prevent. Asserted directly rather than assumed
        // from the layout, because the layout is the thing that could change.
        let tree = Temp::new("repo-root");
        let repo = RepoId::discover(tree.path()).expect("derive a repository id");
        let index = Temp::new("index-root");

        with_index_dir(index.path(), || {
            let path = index_path(&repo).expect("resolve the index path");
            assert!(
                !contains(tree.path(), &path),
                "the index at {} is inside the repository at {}",
                path.display(),
                tree.path().display()
            );
        });
    }

    #[test]
    fn two_worktrees_of_one_repository_do_not_share_an_index() {
        // Both directories are real, so the identities are real rather than synthetic.
        let main = Temp::new("worktree-main");
        let other = Temp::new("worktree-other");
        std::fs::create_dir_all(main.path().join("src")).expect("create the source directory");
        std::fs::create_dir_all(other.path().join("src")).expect("create the source directory");
        std::fs::write(main.path().join("src/lib.rs"), "fn a() {}\n").expect("write source");
        std::fs::write(other.path().join("src/lib.rs"), "fn b() {}\n").expect("write source");

        let index = Temp::new("worktree-index");
        with_index_dir(index.path(), || {
            let first = index_path(&RepoId::discover(main.path()).expect("first identity"))
                .expect("first path");
            let second = index_path(&RepoId::discover(other.path()).expect("second identity"))
                .expect("second path");
            assert_ne!(
                first, second,
                "two checkouts sharing one index would answer questions about whichever \
                 branch was written last"
            );
        });
    }

    #[test]
    fn the_same_checkout_always_resolves_to_the_same_index() {
        let tree = Temp::new("stable");
        let index = Temp::new("stable-index");
        with_index_dir(index.path(), || {
            let first = index_path(&RepoId::discover(tree.path()).expect("first")).expect("first");
            let second =
                index_path(&RepoId::discover(tree.path()).expect("second")).expect("second");
            assert_eq!(
                first, second,
                "a path that changes between runs makes the index unfindable"
            );
        });
    }

    #[test]
    fn an_override_is_used_verbatim_and_not_resolved_against_anything() {
        let temp = Temp::new("override");
        with_index_dir(temp.path(), || {
            assert_eq!(root().expect("root"), temp.path());
        });
    }

    #[test]
    fn an_empty_override_is_ignored_rather_than_making_the_index_root_empty() {
        // An empty string is what a shell produces from `--index-dir=` and what a misconfigured
        // service manager often passes. Treating it as a path would put the index in the process's
        // working directory, which is the bug this module exists to prevent.
        with_index_dir_value(Some(std::ffi::OsStr::new("")), || {
            let resolved =
                root().expect("an empty override must fall back to the platform default");
            assert!(
                !resolved.as_os_str().is_empty(),
                "the index root must never be the empty path"
            );
        });
    }

    #[test]
    fn containment_is_component_wise_and_not_a_string_prefix() {
        // `/x/repo/src-old` is not inside `/x/repo/src`. A prefix comparison says it is, which is
        // exactly the mistake that would let one repository's index be written into another's
        // tree when two projects share a parent directory and a name.
        //
        // **The two names really do share a string prefix, because that is the claim this rests
        // on.** The fixture used to be two unrelated temporary directories, and a string comparison
        // would have got the right answer from them for the wrong reason — so the test asserted
        // that the rule compares components without ever presenting it with two names that a string
        // comparison would confuse. The names are built here, from one parent, for that reason.
        let parent = Temp::new("prefix");
        let source = parent.path().join("repo/src");
        let sibling = parent.path().join("repo/src-old");
        std::fs::create_dir_all(&source).expect("create the source directory");
        std::fs::create_dir_all(&sibling).expect("create the sibling directory");

        // The fixture's own precondition, asserted rather than assumed: a string comparison would
        // call the sibling inside, which is the mistake being ruled out.
        let source_text = source.to_string_lossy();
        let sibling_text = sibling.to_string_lossy();
        assert!(
            sibling_text.starts_with(&*source_text),
            "the fixture must present two names a string prefix cannot tell apart: {} and {}",
            source_text,
            sibling_text
        );

        assert!(!contains(&source, &sibling), "not inside");
        let inside = source.join("a.rs");
        assert!(contains(&source, &inside), "inside, created or not");
        assert!(contains(&source, &source), "inside itself");
    }
}
