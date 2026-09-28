//! Repository identity.
//!
//! # Why a store has to belong to exactly one repository
//!
//! Audit A10: Cortex's `with_store_path` let two worktrees share one store, and each overwrote
//! the other's repository record and document set. A store is therefore keyed to a [`RepoId`],
//! and opening a store against the wrong repository is a loud [`StoreError::WrongRepository`]
//! rather than a merge of two unrelated document sets.
//!
//! # What goes into the hash
//!
//! The repository root **and** the git common directory. The root alone would collide a
//! checkout with a worktree of the same project only if their paths matched, which they never
//! do; including the common directory means a directory that stops being a worktree, or starts
//! being one, gets a different identity rather than silently inheriting the other one's index.
//! Including the root keeps two worktrees of one repository isolated from each other, which is
//! what audit A10 needs.
//!
//! # What the hash is not
//!
//! It is a 128-bit FNV-1a, not a cryptographic digest. That is sufficient and honestly
//! sufficient: it names a directory for the purposes of a cache file, it authenticates nothing,
//! and a collision would need roughly 2^64 repositories to have any meaningful probability of
//! appearing by accident. Anyone needing collision resistance for a different purpose should not
//! reuse this type.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::store::error::StoreError;

/// A stable, opaque identity for one repository checkout.
///
/// Deliberately a newtype over a hex string rather than a path: paths are host-specific, may not
/// be UTF-8, and change when a directory is moved, whereas the identity needs to be storable,
/// comparable, and safe to put in a directory name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoId(String);

impl RepoId {
    /// Derive the identity of the repository whose working tree is `root`.
    ///
    /// Resolves the git common directory when one exists, so the identity follows the repository
    /// rather than the checkout, while remaining distinct per checkout directory.
    pub fn discover(root: &Path) -> Result<Self, StoreError> {
        // Canonicalisation matters: two paths that reach the same directory by different routes
        // must not produce two identities, or a user would silently get two indexes for one tree.
        let canonical = fs::canonicalize(root)
            .map_err(|e| StoreError::Io(format!("cannot resolve {}: {e}", root.display())))?;
        let common = git_common_dir(&canonical);
        Ok(Self(hash(&canonical, common.as_deref())))
    }

    /// The identity as text, for storage in `meta` and for use in a cache directory name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Locate the repository's common git directory.
///
/// Returns `None` outside a repository, which is not an error: Peek indexes a directory it has
/// been pointed at, and identity falls back to the canonical path alone.
fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    // A linked worktree stores a `.git` *file* pointing at its private administrative directory,
    // and that directory contains a `commondir` file naming the real one. Using the private
    // directory instead would make every worktree look like a separate repository.
    let pointer = fs::read_to_string(&dot_git).ok()?;
    let target = pointer
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))?
        .trim();
    let git_dir = PathBuf::from(target);
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        root.join(git_dir)
    };
    match fs::read_to_string(git_dir.join("commondir")) {
        Ok(contents) => {
            let common = contents.trim();
            let common = PathBuf::from(common);
            Some(if common.is_absolute() {
                common
            } else {
                git_dir.join(common)
            })
        }
        Err(_) => Some(git_dir),
    }
}

/// 128-bit FNV-1a over the two paths, rendered as 32 lowercase hex digits.
///
/// The two lengths are separated by a NUL, which cannot appear in either encoded path, so
/// `("ab", "c")` and `("a", "bc")` cannot hash alike.
fn hash(root: &Path, common: Option<&Path>) -> String {
    let mut input: Vec<u8> = Vec::new();
    input.extend_from_slice(b"peek/repo-id/v1");
    input.push(0);
    input.extend_from_slice(&encoded(root));
    input.push(0);
    if let Some(common) = common {
        input.extend_from_slice(&encoded(common));
    }
    fnv1a_128(&input)
}

/// Four independently seeded 64-bit FNV-1a lanes, concatenated.
fn fnv1a_128(bytes: &[u8]) -> String {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    const OFFSETS: [u64; 4] = [
        0xcbf2_9ce4_8422_2325,
        0x9e37_79b9_7f4a_7c15,
        0xff51_afd7_ed55_8ccd,
        0xc4ce_b9fe_1a85_ec53,
    ];
    let mut lanes = OFFSETS;
    for byte in bytes {
        // Every lane consumes every byte. Folding a single 64-bit lane twice would halve the
        // output width for no benefit; running four keeps them independent.
        for lane in &mut lanes {
            *lane ^= u64::from(*byte);
            *lane = lane.wrapping_mul(PRIME);
        }
    }
    let mut hex = String::with_capacity(32);
    for lane in lanes {
        hex.push_str(&format!("{lane:016x}"));
    }
    hex
}

/// The path's own bytes, not a lossy string rendering.
///
/// Audit B1: a `PathBuf` in a serialised struct fails the entire write for a non-UTF-8 path. The
/// identity must therefore be derived from the OS-native encoding, never from `to_str()`.
#[cfg(unix)]
fn encoded(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn encoded(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    // UTF-16, little-endian. Case is preserved verbatim, which is the point: `src/Foo.rs` and
    // `src/foo.rs` are different files and must not be folded together.
    let mut bytes = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::{RepoId, fnv1a_128, hash};
    use crate::store::error::StoreError;
    use crate::store::tests::TempDir;
    use std::fs;
    use std::path::Path;

    #[test]
    fn the_hash_is_deterministic() {
        let dir = TempDir::new("deterministic");
        let first = RepoId::discover(dir.path()).expect("discover");
        let second = RepoId::discover(dir.path()).expect("discover");
        assert_eq!(first, second);
        assert_eq!(
            first.as_str().len(),
            64,
            "four 64-bit lanes, hex encoded — a store key is hashed from a path, so it is widened \
             well past a single lane's 32 bits of birthday resistance"
        );
        assert!(
            first.as_str().chars().all(|c| c.is_ascii_hexdigit()),
            "identity must be hex: {}",
            first.as_str()
        );
    }

    #[test]
    fn different_directories_get_different_identities() {
        let a = TempDir::new("identity-a");
        let b = TempDir::new("identity-b");
        assert_ne!(
            RepoId::discover(a.path()).expect("discover a"),
            RepoId::discover(b.path()).expect("discover b")
        );
    }

    #[test]
    fn path_case_is_preserved_by_the_hash() {
        // The hash sees the path's own bytes, so case never folds. This is the byte-level version
        // of the case-sensitivity tests in the model; the filesystem may not let us create both
        // directories on a case-insensitive volume, so the property is pinned on the encoder.
        assert_ne!(fnv1a_128(b"Src/Main.rs"), fnv1a_128(b"src/main.rs"));
    }

    #[test]
    fn the_two_paths_cannot_be_confused_with_one_another() {
        // Without the NUL separators, ("ab","c") and ("a","bc") would hash identically.
        assert_ne!(
            hash(Path::new("ab"), Some(Path::new("c"))),
            hash(Path::new("a"), Some(Path::new("bc")))
        );
    }

    #[test]
    fn a_repository_is_distinct_from_a_bare_directory_at_the_same_path() {
        let dir = TempDir::new("becomes-repo");
        let bare = RepoId::discover(dir.path()).expect("discover");
        fs::create_dir(dir.path().join(".git")).expect("make it a repository");
        let repo = RepoId::discover(dir.path()).expect("discover");
        assert_ne!(bare, repo, "the git directory is part of the identity");
    }

    #[test]
    fn a_linked_worktree_is_isolated_from_its_repository() {
        // Audit A10 is two worktrees sharing one store. Sharing a *common directory* must not be
        // enough to make them share an identity, or the bug comes straight back.
        let main = TempDir::new("worktree-main");
        let work = TempDir::new("worktree-link");
        let common = main.path().join(".git");
        let private_dir = common.join("worktrees").join("wt");
        fs::create_dir_all(&private_dir).expect("create the worktree admin directory");
        fs::write(private_dir.join("commondir"), "../..").expect("write commondir");
        fs::write(
            work.path().join(".git"),
            format!("gitdir: {}\n", private_dir.display()),
        )
        .expect("write the .git pointer file");

        let main_id = RepoId::discover(main.path()).expect("discover main");
        let work_id = RepoId::discover(work.path()).expect("discover worktree");
        assert_ne!(main_id, work_id, "a worktree must not inherit its repository's index");
    }

    #[test]
    fn discovering_a_missing_directory_is_an_io_error_not_a_panic() {
        let missing = std::env::temp_dir().join("peek-repoid-does-not-exist-9d2f");
        match RepoId::discover(&missing) {
            Err(StoreError::Io(_)) => {}
            other => panic!("expected an Io error, got {other:?}"),
        }
    }
}
