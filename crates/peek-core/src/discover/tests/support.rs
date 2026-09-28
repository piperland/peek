//! Building repository trees to walk.
//!
//! Some things must be built at runtime: symlinks, a real git repository with a linked worktree, a
//! checkout into a directory named `build`. Everything that *can* be committed is committed under
//! `crates/peek-core/tests/fixtures/`, so a reviewer can read a rule and its proof side by side.
//!
//! What follows is only what cannot be committed, and each helper says why.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::discover::DiscoveredFile;

/// A directory tree that removes itself when it goes out of scope.
///
/// A temporary directory that outlives its test is a leak in the developer's temp folder and, on a
/// CI runner, a source of cross-test interference when a test asserts on a directory listing. This
/// type cannot be leaked: it holds its path, and `Drop` removes the tree.
pub struct TempTree {
    path: PathBuf,
}

impl TempTree {
    /// Create an empty tree named after `tag`, uniquely per test and per process.
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "peek-discover-{}-{}-{unique}",
            std::process::id(),
            tag
        ));
        // A leftover tree from a previous run would silently add files to this test's assertions.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temporary tree");
        Self { path }
    }

    /// The tree's root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create a file and every parent directory above it.
    pub fn file(&self, relative: &str, contents: &str) -> &Self {
        self.raw_file(relative, contents.as_bytes())
    }

    /// Create a file with arbitrary bytes, so non-UTF-8 content can be exercised.
    ///
    /// Takes `impl AsRef<[u8]>` rather than `&[u8]` so a test can pass a `&str` literal directly.
    /// A byte-slice parameter makes every ordinary fixture read `as_bytes()`, which is noise in
    /// the common case and hides which fixtures are deliberately not valid UTF-8.
    pub fn raw_file(&self, relative: &str, contents: impl AsRef<[u8]>) -> &Self {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents.as_ref()).expect("write fixture file");
        self
    }

    /// Create an empty directory, so a directory with no files can be walked.
    pub fn dir(&self, relative: &str) -> &Self {
        fs::create_dir_all(self.path.join(relative)).expect("create fixture directory");
        self
    }

    /// Create a symlink at `link` pointing at `target`.
    ///
    /// `target` is interpreted relative to the tree's root, so a test can never accidentally point
    /// a "symlink escape" case at a real system path by accident. [`TempTree::escape_link`] is the
    /// only way to point outside the tree, and it makes doing so explicit.
    ///
    /// Platform-gated because a symlink is a different call on each platform and the suite runs on
    /// both. On Windows this needs either Developer Mode or elevation, which is why the symlink
    /// tests assert on the policy rather than skipping when creation fails: a silently skipped
    /// security test is worse than a failing one.
    pub fn symlink(&self, link: &str, target: &str) -> io::Result<()> {
        let path = self.path.join(link);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create symlink parent directory");
        }
        let target = self.path.join(target);
        platform_symlink(&target, &path)
    }

    /// Create a symlink at `link` pointing at an absolute path outside this tree.
    pub fn escape_link(&self, link: &str, target: &Path) -> io::Result<()> {
        let path = self.path.join(link);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create symlink parent directory");
        }
        platform_symlink(target, &path)
    }

    /// Create a symlink at `link` whose stored target is `target` verbatim.
    ///
    /// Needed for the relative-escape case, where the whole point is the `..` spelling inside the
    /// link. [`Self::symlink`] resolves its target against the tree root and would erase it.
    pub fn raw_symlink(&self, link: &str, target: &str) -> io::Result<()> {
        let path = self.path.join(link);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create symlink parent directory");
        }
        raw_symlink(Path::new(target), &path)
    }

    /// Create a symlink at `link` resolving to a directory inside the tree, for cycle tests.
    pub fn directory_symlink(&self, link: &str, target: &str) -> io::Result<()> {
        let path = self.path.join(link);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create symlink parent directory");
        }
        directory_symlink(&self.path.join(target), &path)
    }

    /// Create a directory symlink at `link` whose stored target is `target` verbatim.
    ///
    /// The relative-escape case needs a link that leaves the tree, which is only meaningful for a
    /// directory: a file link would need a sibling tree whose name the test cannot predict.
    pub fn raw_directory_symlink(&self, link: &str, target: &str) -> io::Result<()> {
        let path = self.path.join(link);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create symlink parent directory");
        }
        directory_symlink(Path::new(target), &path)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        // Best effort: a failure here must not mask a test failure that is already in flight.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn platform_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn platform_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn raw_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn raw_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn directory_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn directory_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// The path of the committed fixture tree.
///
/// Resolved through `CARGO_MANIFEST_DIR` rather than a relative path, because the working directory
/// of a test is not defined to be the crate root — it is wherever the runner was invoked from.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Discover the fixture tree and fail the test with a clear message if it is missing.
///
/// A missing fixture directory is a repository problem, not a test problem, so the message says so
/// rather than surfacing as an empty walk that a later assertion misreads as "no files found".
pub fn mixed_fixture() -> PathBuf {
    let path = fixture("mixed");
    assert!(
        path.is_dir(),
        "missing fixture tree at {}; it is committed under crates/peek-core/tests/fixtures",
        path.display()
    );
    path
}

/// The paths a discovery yielded, as string slices, for order-sensitive comparison.
pub fn paths(files: &[DiscoveredFile]) -> Vec<&str> {
    files.iter().map(|file| file.path.as_str()).collect()
}

/// Whether the given repository-relative path is among the discovered files.
pub fn contains(files: &[DiscoveredFile], wanted: &str) -> bool {
    files.iter().any(|file| file.path.as_str() == wanted)
}
