//! Symlink policy, decided explicitly instead of inherited.
//!
//! # The defect this fixes
//!
//! Cortex read `walkdir::DirEntry::file_type()`, which is **symlink metadata**. For a symlink to a
//! source file, `is_file()` is therefore `false`, and the file was never indexed. A monorepo that
//! shares code by symlinking — a common layout — indexed partially and invisibly: no error, no
//! warning, just missing files. The same call also meant a symlink to a *directory* was skipped
//! without anyone deciding that skipping it was correct.
//!
//! # The decision
//!
//! Peek's default is [`SymlinkPolicy::FollowFilesInsideRoot`], and it is three rules, not one:
//!
//! 1. **A file symlink that resolves inside the repository is followed and indexed.** The
//!    alternative silently loses files, which is the defect above. Monorepo layouts depend on it.
//! 2. **A symlink that resolves outside the repository root is refused.** This is a path-traversal
//!    risk, not a stylistic question. A repository containing `src/leak.rs -> /etc/passwd` must
//!    not cause the indexer to read a file outside the tree the user asked about — and an agent
//!    that can be walked out of the repository by a file it did not write is a security defect, not
//!    a bug report. Refusal is counted and reported with the resolved target so the situation is
//!    visible rather than silent.
//! 3. **A directory symlink is never descended into.** This is what makes a cycle impossible
//!    *by construction* rather than merely unlikely. A directory link pointing back at its own
//!    ancestor is a real pattern in checked-out trees — `node_modules/self`, a `.venv` that links
//!    back to the project — and depth limits do not fix it, they just move the point at which it
//!    fails. Refusing to descend costs nothing: the target is reachable through its real path
//!    anyway, or it is outside the repository and rule 2 already applies.
//!
//! # Containment is checked on canonical paths
//!
//! The containment test compares `fs::canonicalize` output on both sides. That resolves `.` and
//! `..` in the link target *and* every symlink along the way, so a link cannot escape the root by
//! spelling. It also means the root must be canonicalised too, which is why
//! [`crate::discover::FileDiscovery::discover`] canonicalises once and hands the same path to the
//! identity, the containment test and the relative-path computation. Comparing a canonical path
//! against a non-canonical one on Windows is a case-sensitive comparison against a `\\?\`-prefixed
//! path, and would report a legitimate link as an escape.
//!
//! [`SymlinkPolicy::Skip`] exists for callers that want strictness — an air-gapped analysis, or a
//! repository where a symlink's presence is itself the finding. It is not the default because the
//! default has to be the one that does not lose files.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How a walk treats symbolic links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymlinkPolicy {
    /// Index file symlinks that resolve inside the repository root, refuse any that resolve
    /// outside it, and never descend into a directory symlink. The default.
    #[default]
    FollowFilesInsideRoot,
    /// Index nothing through a symlink. Every link is counted and reported.
    Skip,
}

impl SymlinkPolicy {
    /// The stable lowercase identifier used in output.
    pub const fn as_str(self) -> &'static str {
        match self {
            SymlinkPolicy::FollowFilesInsideRoot => "follow_files_inside_root",
            SymlinkPolicy::Skip => "skip",
        }
    }
}

impl fmt::Display for SymlinkPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a walk decided to do with one symlink.
///
/// This is an internal decision type. It is not public because a caller should not be branching on
/// it: the branches already surface as distinct, named counters in
/// [`crate::discover::DiscoveryStats`] and as [`crate::discover::WalkIssue`] entries. Making the
/// decision type public would give callers a second, easier-to-misuse way to react to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// The link resolves to a regular file inside the root. Index it at this absolute path.
    File { absolute: PathBuf },
    /// The link resolves to a directory inside the root. Not descended into, so no cycle is
    /// possible.
    Directory,
    /// The link resolves outside the repository root. Refused as a path-traversal risk.
    Escapes { target: PathBuf },
    /// The link could not be read, or its target does not exist.
    Unresolvable { detail: String },
    /// The policy is [`SymlinkPolicy::Skip`].
    Skipped,
}

impl SymlinkPolicy {
    /// Resolve one symlink under the canonical `root`.
    ///
    /// `root` must be the same canonical path the walk was rooted at. A non-canonical root makes
    /// the containment test below unsound on Windows, where `starts_with` compares
    /// `\\?\`-prefixed paths case-sensitively and would report a legitimate link as an escape.
    pub(crate) fn resolve(self, root: &Path, link: &Path) -> Resolution {
        if self == SymlinkPolicy::Skip {
            return Resolution::Skipped;
        }

        // Canonicalising the link resolves `.`, `..` and every intermediate symlink, so the
        // containment test below cannot be defeated by spelling.
        let target = match fs::canonicalize(link) {
            Ok(target) => target,
            Err(error) => {
                return Resolution::Unresolvable {
                    detail: error.to_string(),
                };
            }
        };

        // The containment test comes before the type test on purpose. A link out of the
        // repository is refused whether it points at a file, a directory, or a device.
        if !target.starts_with(root) {
            return Resolution::Escapes { target };
        }

        match fs::metadata(&target) {
            Ok(metadata) if metadata.is_dir() => Resolution::Directory,
            Ok(metadata) if metadata.is_file() => Resolution::File { absolute: target },
            // Sockets, fifos and devices are not indexable, but they are inside the root and so
            // they are not an escape. Reporting them as unresolvable keeps the outcome honest
            // without inventing a traversal that did not happen.
            Ok(_) => Resolution::Unresolvable {
                detail: "target is neither a regular file nor a directory".to_owned(),
            },
            Err(error) => Resolution::Unresolvable {
                detail: error.to_string(),
            },
        }
    }
}
