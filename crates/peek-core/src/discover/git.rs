//! Repository identity, including worktree isolation.
//!
//! # The defect this fixes
//!
//! Cortex had **zero** git awareness — no commit, no branch, no common directory, nothing. Two
//! consequences, both silent:
//!
//! * `with_store_path` let two worktrees of one repository point at a single store. Each overwrote
//!   the other's `repo_path` and the whole document set, so neither was ever correct and neither
//!   reported an error.
//! * `git checkout` with the watcher stopped left a stale index that `health()` reported as
//!   healthy.
//!
//! Discovery fixes the first. The second belongs to the indexer and the store, not here, but
//! [`RepoIdentity`] gives it what it needs: a cheap way to ask "is this the same checkout I
//! looked at before?".
//!
//! # Why the git common directory
//!
//! The common directory is the directory that is **shared** by every worktree of a repository. It
//! is what `git rev-parse --git-common-dir` prints, resolved to an absolute path.
//!
//! It is the right input for a store key, and the reason is worth stating precisely because the
//! intuitive argument is the wrong one. The argument is *not* "two worktrees have different common
//! directories" — that is false, and the reason they must be keyed apart is subtler:
//!
//! * A linked worktree's checkout path is its own root directory. Two worktrees of one repository
//!   have two different roots, and each root is a distinct thing the user pointed Peek at.
//! * The common directory is what tells you those two roots are *the same repository*, so a
//!   revision in one can invalidate the other, and so `peek status` can report a single repository
//!   with two indexes rather than two unrelated repositories.
//!
//! Folding both into the id therefore gives an id that is distinct when the *checkout* is distinct
//! — which is what keeps two worktrees apart — and carries the *repository* alongside it, so a
//! caller can report one repository with two indexes rather than two unrelated repositories. A
//! common-directory-only id would be actively wrong: it is identical for every worktree, so it
//! would hand both worktrees the same store key, which is the exact defect being fixed. A
//! root-only id would be safe for the worktree case but would silently conflate two unrelated
//! repositories that happen to share a volume.
//!
//! # Why the git CLI and not `git2`
//!
//! Shelling out to `git` costs one process spawn per walk — a few milliseconds, once, against a
//! walk that reads every source file — and keeps the dependency surface free of a full libgit2
//! binding. `git2` would be the wrong call for a different reason as well: it vendors a C library
//! and a C toolchain, and the target platform is one where the build is already the hard part.
//!
//! The one cost worth naming: `git` must be on `PATH`. When it is not, discovery falls back to a
//! path hash and says so through [`RepoIdSource`], rather than failing. An indexer that cannot
//! index a directory that is not a git repository would be useless, and a checkout is not the only
//! thing an agent asks questions about.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// A stable, collision-resistant identity for one repository checkout.
///
/// Two `RepoId`s are equal exactly when they name the same checkout on the same machine. This is
/// the value a store is keyed by, and the reason two worktrees of one repository cannot collide.
///
/// It is a hash, not a path. A path would be longer, would leak the user's directory layout into
/// the store, and would need a separate escaping scheme. A hash is a fixed 16 lowercase hex
/// characters after a one-character source tag, so it is safe in a filename on every platform.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepoId {
    value: String,
    source: RepoIdSource,
}

impl RepoId {
    /// The identity as a string, e.g. `g:3f2a1b0c9d8e7f60`.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Which inputs produced this id.
    ///
    /// Stored on the id rather than recomputed by the caller, so the tag in the string and the
    /// reported source can never disagree.
    pub const fn source(&self) -> RepoIdSource {
        self.source
    }

    /// Compute an identity from a canonical repository root and, when it could be determined, the
    /// resolved git common directory.
    pub fn new(root: &Path, common_dir: Option<&Path>) -> Self {
        let source = match common_dir {
            Some(_) => RepoIdSource::GitCommonDir,
            None => RepoIdSource::Path,
        };
        let common = common_dir.unwrap_or(Path::new(""));
        // Each part is NUL-terminated so that ("ab", "c") and ("a", "bc") cannot collide. Both
        // spellings are legitimate directory and file names, and the root is very often a prefix
        // of the common directory's own components, so the separator is not optional.
        let hash = fnv1a64(&[
            root.as_os_str().as_encoded_bytes(),
            b"\0",
            common.as_os_str().as_encoded_bytes(),
            b"\0",
        ]);
        Self {
            value: format!("{}:{hash:016x}", source.tag()),
            source,
        }
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

/// Where a [`RepoId`]'s inputs came from.
///
/// Recorded rather than assumed, because the fallback is weaker in a way a caller can act on. A
/// path-only id still separates two checkouts correctly, so the worktree guarantee holds either way;
/// what it loses is the ability to say the two checkouts are the *same repository*, which is what a
/// caller needs in order to report one repository with two indexes rather than two unrelated
/// projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoIdSource {
    /// The repository root and the git common directory both contributed, so the id identifies a
    /// checkout of a known repository.
    GitCommonDir,
    /// Only the repository root contributed, because git was unavailable or the path is not a
    /// repository.
    Path,
}

impl RepoIdSource {
    /// The single-character tag that prefixes the id, so a store key is self-describing.
    const fn tag(self) -> char {
        match self {
            RepoIdSource::GitCommonDir => 'g',
            RepoIdSource::Path => 'p',
        }
    }

    /// The stable lowercase identifier used in output.
    pub const fn as_str(self) -> &'static str {
        match self {
            RepoIdSource::GitCommonDir => "git_common_dir",
            RepoIdSource::Path => "path",
        }
    }
}

impl fmt::Display for RepoIdSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A repository checkout, with the identity derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    id: RepoId,
    source: RepoIdSource,
    common_dir: Option<PathBuf>,
}

impl RepoIdentity {
    /// Derive the identity of a canonical repository root.
    ///
    /// `root` must already be canonical. The walk canonicalises once and passes that path here so
    /// the identity, the symlink containment test and the relative-path computation all agree on
    /// one spelling of the root; a non-canonical root would make all three disagree.
    pub fn identify(root: &Path) -> Self {
        let common_dir = git_common_dir(root);
        let id = RepoId::new(root, common_dir.as_deref());
        Self {
            // Taken from the id rather than recomputed, so the two can never disagree.
            source: id.source(),
            id,
            common_dir,
        }
    }

    /// The identity, and the value a store is keyed by.
    pub fn id(&self) -> &RepoId {
        &self.id
    }

    /// Where the id's inputs came from.
    pub fn source(&self) -> RepoIdSource {
        self.source
    }

    /// The resolved git common directory, or `None` when it could not be determined.
    pub fn common_dir(&self) -> Option<&Path> {
        self.common_dir.as_deref()
    }
}

/// The resolved absolute path of the repository's git common directory.
///
/// Returns `None` when git is unavailable, the path is not inside a repository, or git fails for
/// any other reason. `git rev-parse` prints the common directory relative to its working
/// directory, so a relative answer is resolved against `root` before being canonicalised — a
/// relative path would otherwise make two worktrees hash identically and reintroduce exactly the
/// collision this is meant to prevent.
fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--git-common-dir")
        // git's own diagnostics are noise here: a non-repository prints to stderr and exits
        // non-zero, which is an expected outcome, not an error worth surfacing.
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let printed = String::from_utf8(output.stdout).ok()?;
    let printed = printed.trim();
    if printed.is_empty() {
        return None;
    }

    let printed = Path::new(printed);
    let absolute = if printed.is_absolute() {
        printed.to_path_buf()
    } else {
        root.join(printed)
    };
    fs::canonicalize(absolute).ok()
}

/// FNV-1a, 64-bit.
///
/// Not a cryptographic hash and not trying to be. It is here because the default hasher's stability
/// is explicitly not guaranteed across Rust versions, and a store key that changed between compiler
/// releases would orphan every existing index. FNV-1a is four lines, has no dependency, and its
/// output is fixed forever.
///
/// The inputs are length-delimited by a trailing NUL so that `("ab", "c")` and `("a", "bc")` cannot
/// collide. Both of those are legitimate directory and file names.
fn fnv1a64(parts: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}
