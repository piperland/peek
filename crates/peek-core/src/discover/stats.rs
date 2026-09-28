//! What a walk did, in numbers that mean something.
//!
//! `doctor`, the benchmark harness and the incremental-update decision all read this struct, which
//! makes two properties non-negotiable: it is **complete**, so a reader never has to guess where
//! files went, and it is **honest**, so no number here is invented.
//!
//! # The one thing this struct does not count
//!
//! Entries removed by an ignore file. The `ignore` crate prunes them inside its own iterator
//! before the entry is ever handed to the walk hook, so they cannot be observed, only inferred —
//! and a count that is inferred rather than measured is exactly the kind of number that makes a
//! report confidently wrong. [`DiscoveryStats::excluded`] counts what Peek's own
//! [`crate::discover::ExcludeSet`] rejected, which *is* measurable, because that rejection happens
//! in a hook that sees the entry first.
//!
//! The visible consequence is that [`DiscoveryStats::entries_visited`] means "entries the walker
//! produced", not "inodes on disk". A repository with a large `node_modules` in its `.gitignore`
//! has a low `entries_visited` and a high `files_yielded` relative to its size on disk. That is
//! correct, not a bug, and it is stated here rather than left to be discovered.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::classify::Classification;
use crate::model::Language;

/// The counts behind one discovery walk.
///
/// Every field is a real observation. There is no field whose value is a guess, a default, or a
/// fallback that hides a failure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryStats {
    /// Entries the walker handed to the walk, including the root itself and every directory.
    ///
    /// This is *not* the number of inodes in the tree. Entries pruned by an ignore file never
    /// reach Peek and are not counted; see the module documentation.
    pub entries_visited: usize,
    /// Of those, the ones that were directories.
    pub directories_visited: usize,
    /// Regular files considered for indexing, after ignore files and the exclusion set removed
    /// directories from consideration. A file counted here went on to be either yielded or skipped
    /// for one of the named reasons below.
    pub files_examined: usize,
    /// Files yielded. This is the indexable file count.
    pub files_yielded: usize,
    /// Total bytes of the yielded files, as measured during the walk.
    pub bytes: u64,
    /// The largest yielded file in bytes. A benchmark reads this to explain a slow parse.
    pub largest_file: u64,

    /// Entries pruned by the [`crate::discover::ExcludeSet`], files and directories together.
    ///
    /// Measured exactly. It is the only exclusion counter here because it is the only exclusion
    /// Peek performs itself.
    pub excluded: usize,
    /// Files skipped because their extension maps to no [`Language`].
    pub unsupported_extension: usize,
    /// Files skipped for exceeding the configured size cap.
    pub too_large: usize,
    /// Files skipped because their contents are not valid UTF-8.
    ///
    /// This counter is the reason one non-UTF-8 file can no longer abort a repository. Cortex
    /// propagated the decode failure out of `build_full` and persisted nothing at all; here the
    /// file is skipped and the count is reported.
    pub not_utf8: usize,
    /// Files skipped because they could not be read, stat'd, or resolved. Each has a matching
    /// [`WalkIssue`].
    pub unreadable: usize,
    /// Entries skipped because they are neither a regular file nor a directory: sockets, fifos,
    /// devices. Counted so the arithmetic over `entries_visited` closes.
    pub irregular_skipped: usize,
    /// Files skipped because their path duplicated an already-yielded one under the effective
    /// [`crate::discover::CaseSensitivity`]. Each has a matching [`WalkIssue`].
    pub duplicates: usize,

    /// File symlinks resolved inside the repository root and indexed.
    pub symlinks_followed: usize,
    /// Symlinks that resolved outside the repository root and were refused as a path-traversal
    /// risk. Each has a matching [`WalkIssue`] naming the target.
    pub symlink_escapes: usize,
    /// Directory symlinks not descended into, so that no cycle is possible. See
    /// [`crate::discover::SymlinkPolicy`].
    pub symlink_directories_skipped: usize,
    /// Symlinks not followed because the policy is [`crate::discover::SymlinkPolicy::Skip`].
    pub symlink_skipped: usize,

    /// Files yielded per language, sorted by language.
    pub by_language: BTreeMap<Language, usize>,
    /// Files yielded per classification, sorted by classification. This is the breakdown
    /// `doctor` prints when it tells an agent what to ignore.
    pub by_classification: BTreeMap<Classification, usize>,

    /// The length of [`DiscoveryReport::issues`](super::DiscoveryReport::issues).
    pub walk_issues: usize,
}

impl DiscoveryStats {
    /// Whether the walk yielded nothing.
    pub fn is_empty(&self) -> bool {
        self.files_yielded == 0
    }

    /// Entries the walk examined but did not yield, across every skip reason.
    ///
    /// This is the number a reader wants when reconciling a file count against a directory
    /// listing, so it is provided rather than left for each caller to sum.
    pub fn skipped(&self) -> usize {
        self.excluded
            + self.unsupported_extension
            + self.too_large
            + self.not_utf8
            + self.unreadable
            + self.irregular_skipped
            + self.duplicates
            + self.symlink_directories_skipped
            + self.symlink_skipped
    }

    /// A one-line summary for logs.
    pub fn summary(&self) -> String {
        format!(
            "{} yielded, {} skipped ({} excluded, {} unsupported, {} too large, \
             {} not utf-8, {} unreadable, {} duplicate) from {} entries",
            self.files_yielded,
            self.skipped(),
            self.excluded,
            self.unsupported_extension,
            self.too_large,
            self.not_utf8,
            self.unreadable,
            self.duplicates,
            self.entries_visited,
        )
    }
}

impl fmt::Display for DiscoveryStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

/// One entry that could not be used, without stopping the walk.
///
/// This is the mechanism that replaces Cortex's per-file failure propagation. A walk records
/// problems and continues; it never aborts a repository because of one file, and it never reports
/// success without accounting for the files it dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkIssue {
    /// The path, repository-relative where one could be computed, else the literal `<walk>`.
    pub path: String,
    /// What went wrong.
    pub reason: WalkIssueReason,
}

impl fmt::Display for WalkIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            WalkIssueReason::Unreadable { detail } => write!(f, "{}: {detail}", self.path),
            WalkIssueReason::OutsideRepository => {
                write!(f, "{}: resolves outside the repository root", self.path)
            }
            WalkIssueReason::NonUtf8Path => {
                write!(f, "{}: path is not valid UTF-8", self.path)
            }
            WalkIssueReason::SymlinkEscapes { target } => {
                write!(
                    f,
                    "{}: symlink escapes the repository to {target}",
                    self.path
                )
            }
            WalkIssueReason::UnresolvableSymlink { detail } => {
                write!(f, "{}: {detail}", self.path)
            }
            WalkIssueReason::Duplicate => {
                write!(
                    f,
                    "{}: duplicate under the filesystem's case sensitivity",
                    self.path
                )
            }
        }
    }
}

/// Why one entry was skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WalkIssueReason {
    /// The entry could not be read: permissions, a broken symlink, a file removed mid-walk, or an
    /// ignore file that failed to parse.
    Unreadable {
        /// The underlying message.
        detail: String,
    },
    /// The entry resolved outside the repository root, so it is not a repository file.
    OutsideRepository,
    /// The entry's path is not valid UTF-8, so it cannot be a [`crate::model::RepoPath`].
    ///
    /// Reported rather than dropped: on Unix such paths are real, and a caller debugging a
    /// surprising file count deserves to know one exists.
    NonUtf8Path,
    /// A symlink resolved outside the repository root and was refused.
    SymlinkEscapes {
        /// The resolved target, absolute.
        target: String,
    },
    /// A symlink could not be resolved at all, typically because its target does not exist.
    UnresolvableSymlink {
        /// The underlying message.
        detail: String,
    },
    /// The path duplicated one already yielded, under the filesystem's case sensitivity.
    Duplicate,
}
