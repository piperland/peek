//! Deciding what a batch of filesystem events means.
//!
//! # Why this is separated from the watcher
//!
//! Because the interesting part of a file watcher is not the watching. It is the *decision*: given
//! forty-three events that arrived in one burst, which files actually need re-extracting, which
//! need removing, and which are none of the watcher's business. That decision is pure, and a pure
//! decision can be tested exhaustively and without a filesystem, which is the difference between a
//! watcher that is correct and a watcher that is correct on the machine it was written on.
//!
//! The watcher in [`crate::watch::native`] is deliberately thin: it turns OS notifications into
//! paths, hands them here, and applies the answer.
//!
//! # The properties that matter
//!
//! **Coalescing.** Saving a file in an editor produces a create, several writes, a rename and a
//! delete. Re-extracting once per event is not slow, it is *wrong*: it churns the store's
//! generation for every intermediate state, and an index that has been through forty generations
//! has a log to replay. One path, once, per batch.
//!
//! **A rename is a removal and an addition, and needs no special case.** That is not a trick, it
//! is what the filesystem does, and handling it explicitly is how implementations end up
//! double-counting.
//!
//! **A path outside the repository is not ours to act on.** It is counted, not ignored silently,
//! because a path arriving from outside the root means the watcher is watching something it was
//! not asked to watch.
//!
//! **Deleted means removed, even if the delete arrives before the create.** A build that indexed
//! the file and then lost it must not keep answering questions about it, and the index has to be
//! able to say the symbol is gone rather than leaving a row that points nowhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a path in a batch requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// Re-read and re-extract this path.
    ///
    /// Chosen over `Remove` even when the file is absent, because `refresh` already handles a
    /// missing file by removing its rows — and combining the two into one call keeps the decision
    /// a single value rather than two that have to agree.
    Reindex,
    /// This path is not ours: excluded, outside the root, or not a file type we index.
    Ignore,
}

impl Action {
    /// The word a status line prints.
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::Reindex => "reindex",
            Action::Ignore => "ignore",
        }
    }
}

/// The decision for one path, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub path: PathBuf,
    pub action: Action,
    /// A short reason, kept so a caller that *does* report can say why a path was skipped rather
    /// than leaving the user to infer it from a count.
    pub reason: &'static str,
}

/// The outcome of planning one batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Paths to hand to `indexer::refresh`, sorted and deduplicated.
    ///
    /// Sorted so two identical batches produce two identical plans, and deduplicated so a burst of
    /// forty-three events for one file produces one entry.
    pub reindex: Vec<PathBuf>,
    /// Paths that were not ours, with the reason.
    pub ignored: Vec<Decision>,
    /// Paths that arrived from outside the repository root.
    ///
    /// Counted separately from `ignored` because it means the *watcher* is misconfigured, which is
    /// a different problem from a path being excluded by policy.
    pub outside_root: Vec<PathBuf>,
}

impl Plan {
    /// Whether the batch would change nothing.
    ///
    /// The caller uses this to avoid a pointless transaction. A generation that advances for an
    /// empty batch makes the generation counter useless as a change signal, which is the one
    /// thing it is for.
    pub fn is_empty(&self) -> bool {
        self.reindex.is_empty()
    }

    /// One line, for `peek status` and the MCP `index_status` primitive.
    pub fn summary(&self) -> String {
        format!(
            "{} path(s) to re-index, {} ignored, {} from outside the repository",
            self.reindex.len(),
            self.ignored.len(),
            self.outside_root.len()
        )
    }
}

/// Turn a burst of raw event paths into a plan.
///
/// `root` is the repository root; a path that is not under it is counted as outside rather than
/// silently dropped. `is_indexable` decides whether an extension is one we extract, so that a
/// `.md` edit does not cost a transaction. Both are injected rather than discovered so this stays
/// pure and so a caller can apply whatever policy it actually has.
pub fn plan_batch<F>(root: &Path, events: &[PathBuf], mut is_indexable: F) -> Plan
where
    F: FnMut(&Path) -> bool,
{
    // A BTreeMap rather than a HashSet: the output order is part of the contract, and
    // "sorted" is checkable in a test in a way "whatever order the set had" is not.
    let mut unique: BTreeMap<PathBuf, Action> = BTreeMap::new();
    let mut outside_root: Vec<PathBuf> = Vec::new();

    for path in events {
        if !path.starts_with(root) {
            // A path outside the root is a fact about the watcher's configuration. Counted, and
            // kept out of the re-index list: acting on it would mean touching files the user did
            // not ask us to read.
            if !outside_root.contains(path) {
                outside_root.push(path.clone());
            }
            continue;
        }
        if !is_indexable(path) {
            continue;
        }
        // A later event for the same path replaces an earlier one, and both map to `Reindex`, so
        // the entry is written once. `Reindex` is the only outcome that survives coalescing
        // because `indexer::refresh` handles presence and absence in one path.
        unique.insert(path.clone(), Action::Reindex);
    }

    let mut ignored = Vec::new();
    for path in events {
        if !path.starts_with(root) {
            continue;
        }
        if is_indexable(path) {
            continue;
        }
        if ignored
            .iter()
            .any(|decision: &Decision| decision.path == *path)
        {
            continue;
        }
        ignored.push(Decision {
            path: path.clone(),
            action: Action::Ignore,
            reason: "not a file type this build extracts",
        });
    }

    outside_root.sort();
    Plan {
        reindex: unique.into_keys().collect(),
        ignored,
        outside_root,
    }
}

/// Coalesce raw event paths into batches, separated by a quiet period.
///
/// The rule is deliberately simple and deliberately lossy in one direction: a batch closes once
/// no new event has arrived for `quiet_for`, and **at most one** batch is ever held. An event that
/// arrives while a batch is being applied joins the *next* batch rather than growing this one,
/// which is what keeps a slow resolution pass from turning into an unbounded queue.
///
/// `flush` is called with a batch; it returns the number of paths in it, and the caller uses that
/// for nothing more than logging. It is a function rather than a trait so this is testable with a
/// closure and has no lifecycle of its own.
#[derive(Debug, Clone)]
pub struct Debouncer {
    quiet_for: std::time::Duration,
    pending: Vec<PathBuf>,
    /// When the most recent event arrived. `None` while the batch is empty.
    last_event: Option<std::time::Instant>,
    /// Set when applying a batch fails. Reported by the next drain rather than swallowed, because
    /// a watcher that silently drops a failed refresh produces a stale index that reports itself
    /// as current.
    last_failure: Option<String>,
}

impl Debouncer {
    /// A debouncer that closes a batch after `quiet_for` with no further event.
    pub fn new(quiet_for: std::time::Duration) -> Self {
        Self {
            quiet_for,
            pending: Vec::new(),
            last_event: None,
            last_failure: None,
        }
    }

    /// The quiet period this debouncer uses.
    pub fn quiet_for(&self) -> std::time::Duration {
        self.quiet_for
    }

    /// How many raw paths are waiting. Counts events, not distinct paths: the coalescing happens
    /// when the batch is planned, and this is the number that says "a burst is in progress".
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// How long has passed since the last event, or `None` while the batch is empty.
    pub fn quiet_for_so_far(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.last_event.map(|at| now.saturating_duration_since(at))
    }

    /// Whether the last applied batch failed, and why.
    ///
    /// Reported once by the next `drain`. A watcher that retries silently forever is worse than
    /// one that says "I could not apply those four paths", because the first looks healthy.
    pub fn last_failure(&self) -> Option<&str> {
        self.last_failure.as_deref()
    }

    /// Note that applying a batch failed.
    pub fn note_failure(&mut self, reason: impl Into<String>) {
        self.last_failure = Some(reason.into());
    }

    /// Record one or more events, restarting the quiet period.
    pub fn record<I, P>(&mut self, paths: I)
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let paths: Vec<PathBuf> = paths.into_iter().map(Into::into).collect();
        if paths.is_empty() {
            return;
        }
        self.pending.extend(paths);
        self.last_event = Some(std::time::Instant::now());
    }

    /// Close the batch if the quiet period has elapsed.
    ///
    /// Returns `None` when the batch is still open, which is the common case: this is called on a
    /// timer rather than on an event, so most calls do nothing.
    pub fn drain_if_quiet(&mut self, now: std::time::Instant) -> Option<Vec<PathBuf>> {
        if self.pending.is_empty() {
            return None;
        }
        let quiet = now.saturating_duration_since(self.last_event?);
        if quiet < self.quiet_for {
            return None;
        }
        self.close()
    }

    /// Close the batch regardless of elapsed time, which is what shutdown does.
    ///
    /// A batch that is still open at shutdown is a batch whose changes are **not** in the index,
    /// so this is the difference between a clean exit and a stale one.
    pub fn flush(&mut self) -> Option<Vec<PathBuf>> {
        if self.pending.is_empty() {
            return None;
        }
        self.close()
    }

    fn close(&mut self) -> Option<Vec<PathBuf>> {
        self.last_event = None;
        Some(std::mem::take(&mut self.pending))
    }
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary())
    }
}

#[cfg(test)]
mod tests;
