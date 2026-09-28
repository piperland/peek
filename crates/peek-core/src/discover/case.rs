//! Case sensitivity, and why it is not a boolean.
//!
//! # The defect this fixes
//!
//! Cortex's discovery skip list was matched **case-sensitively** while the store key was
//! **lowercased**. On NTFS that is a straight-up mismatch: `Target/` and `NODE_MODULES/` were
//! indexed and then folded into the store, so the exclusion policy that was supposed to keep build
//! output out of the index did nothing on the platform where most Windows development happens. The
//! two decisions were made in different places and never reconciled.
//!
//! Peek resolves this in one direction permanently: **case is never folded in output.**
//! [`crate::model::RepoPath`] preserves the case it was given, and every path discovery returns is
//! a `RepoPath`. No component of Peek lowercases a path.
//!
//! What remains is a genuine question with a genuine answer that varies by volume: *are two paths
//! that differ only in case the same file?* Getting it wrong causes a specific silent failure —
//! a repository containing both `src/Foo.rs` and `src/foo.rs` yields two paths that look
//! identical, and any downstream consumer that folds case keeps one of them without saying so.
//!
//! # Detection, without writing anything
//!
//! Probing a real filesystem is the only honest answer, and it does not require creating a file.
//! Take the repository root, flip the ASCII case of one of its components, and ask whether the
//! altered path still resolves to the same entry:
//!
//! * On ext4, `/home/dev/Peek` with a flipped `peek` does not exist, so the volume distinguishes
//!   the two: [`CaseSensitivity::Sensitive`].
//! * On NTFS or APFS, the flipped name resolves to the same entry. `fs::canonicalize` returns the
//!   volume's own spelling of the name on both sides, so the two results are equal:
//!   [`CaseSensitivity::Insensitive`].
//! * If a volume genuinely holds both `Peek` and `peek` as distinct entries, the flipped path
//!   resolves to a *different* entry and the comparison correctly reports
//!   [`CaseSensitivity::Sensitive`].
//!
//! Nothing is created, nothing is deleted, and the repository is never written to. That matters:
//! an indexer that drops probe files into the tree it is indexing is not acceptable, and it would
//! also make discovery non-idempotent.
//!
//! Detection cannot answer for a component whose name contains no ASCII letter, so the probe walks
//! up towards the filesystem root. When nothing can be flipped, or nothing can be read, detection
//! reports [`CaseSensitivity::Insensitive`] — the conservative direction, which surfaces a
//! duplicate that may not exist rather than hiding one that does.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Whether a filesystem treats two paths differing only in case as the same file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseSensitivity {
    /// Paths differing only in case are different files, as on ext4, XFS and Btrfs.
    Sensitive,
    /// Paths differing only in case are the same file, as on NTFS, APFS and HFS+.
    Insensitive,
}

impl CaseSensitivity {
    /// The stable lowercase identifier used in output.
    pub const fn as_str(self) -> &'static str {
        match self {
            CaseSensitivity::Sensitive => "sensitive",
            CaseSensitivity::Insensitive => "insensitive",
        }
    }

    /// Whether paths differing only in case are distinct.
    pub const fn is_sensitive(self) -> bool {
        matches!(self, CaseSensitivity::Sensitive)
    }

    /// Whether paths differing only in case are the same file.
    pub const fn is_insensitive(self) -> bool {
        matches!(self, CaseSensitivity::Insensitive)
    }

    /// The key under which a path is recorded for duplicate detection.
    ///
    /// On a case-insensitive volume two differently-cased paths are one file, so they must collide
    /// here. On a case-sensitive volume they are two files, so they must not. This is the only
    /// place in Peek where case is folded, and what it folds into is a transient key used to
    /// detect duplicates — never a stored path.
    pub fn key(self, path: &str) -> String {
        match self {
            CaseSensitivity::Sensitive => path.to_owned(),
            CaseSensitivity::Insensitive => path.to_ascii_lowercase(),
        }
    }

    /// Detect the case sensitivity of the volume holding `root`.
    ///
    /// Returns [`CaseSensitivity::Insensitive`] if the probe cannot be run. See the module
    /// documentation for why that is the safe direction to fail in.
    pub fn detect(root: &Path) -> Self {
        let mut candidate = Some(root);
        while let Some(path) = candidate {
            if let Some(detected) = probe(path) {
                return detected;
            }
            candidate = path.parent();
        }
        CaseSensitivity::Insensitive
    }
}

impl std::fmt::Display for CaseSensitivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ask whether a case-flipped spelling of `path` resolves to the same entry.
///
/// Returns `None` when this component cannot be used as a probe, so the caller can try the parent.
fn probe(path: &Path) -> Option<CaseSensitivity> {
    let flipped = flip_ascii_case(path.file_name()?.to_str()?)?;
    let mut candidate = path.to_path_buf();
    candidate.set_file_name(flipped);

    match (fs::canonicalize(path), fs::canonicalize(&candidate)) {
        // The volume reports the same entry under both spellings.
        (Ok(original), Ok(mirrored)) if original == mirrored => {
            Some(CaseSensitivity::Insensitive)
        }
        // Either the flipped spelling does not resolve at all, or it resolves to a genuinely
        // different entry because the volume holds both. Both mean the volume tells them apart.
        (Ok(_), _) => Some(CaseSensitivity::Sensitive),
        // This component could not be read. Try the parent.
        (Err(_), _) => None,
    }
}

/// The ASCII-case-flipped spelling of `name`, or `None` when flipping changes nothing.
///
/// Only ASCII letters are flipped. A non-ASCII name may still be probed through a parent
/// component, and a name that is entirely digits is never a useful probe.
fn flip_ascii_case(name: &str) -> Option<OsString> {
    let flipped: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else if character.is_ascii_lowercase() {
                character.to_ascii_uppercase()
            } else {
                character
            }
        })
        .collect();
    if flipped == name {
        None
    } else {
        Some(OsString::from(flipped))
    }
}
