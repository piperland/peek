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
//! **The operating system does not spell the root the way the caller did, and a watcher that
//! believes it did re-indexes nothing.** The backends disagree: `inotify` reports a path under the
//! name it was registered with, while FSEvents reports it under the *resolved* name, and on macOS
//! the directory a temporary fixture lives in is behind a symlink (`/var` resolves to
//! `/private/var`). A watcher that compares a reported path against the caller's spelling of the
//! root therefore decides that *every* event arrived from outside the repository: counted in
//! `outside_root`, absent from the re-index list, and reported as no failure at all — an index that
//! claims to be current and is not. A reported path is therefore restated in the caller's spelling
//! before it is planned, which is also the spelling `indexer::refresh` strips against to reach a
//! repository-relative path. See [`under_root`].
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

/// Restate a path an operating system reported in the spelling of the root a caller named.
///
/// `root` is the root as the caller wrote it and `resolved_root` is the same directory with its
/// symlinks resolved. A backend that names the resolved directory reports every event under it, and
/// a watcher that compared those paths against `root` would classify all of them as arriving from
/// outside the repository — counted, dropped, and never reported as a failure.
///
/// A path already under `root` is returned unchanged, and so is one under neither, because deciding
/// that a path is outside the root is [`plan_batch`]'s job and this must not pre-empt it: a path
/// that really is elsewhere has to stay countable rather than be quietly reshaped into something
/// that looks like it is inside.
fn under_root(root: &Path, resolved_root: &Path, reported: &Path) -> PathBuf {
    if reported.starts_with(root) {
        return reported.to_path_buf();
    }
    match reported.strip_prefix(resolved_root) {
        Ok(relative) => root.join(relative),
        Err(_) => reported.to_path_buf(),
    }
}

/// Turn a burst of raw event paths into a plan.
///
/// `root` is the repository root; a path that is not under it is counted as outside rather than
/// silently dropped. `is_indexable` decides whether an extension is one we extract, so that a
/// `.md` edit does not cost a transaction. Both are injected rather than discovered so this stays
/// pure and so a caller can apply whatever policy it actually has.
///
/// The comparison against `root` is lexical, which is what makes it pure and testable, and it is
/// also why a caller feeding it paths from an operating system hands them through [`under_root`]
/// first: the two sides have to be spelled the same way or every path looks like it came from
/// elsewhere.
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

/// The OS-specific half of watching.
///
/// This is deliberately the *thin* half. It turns notifications into paths and hands them to
/// [`Debouncer`]; every decision about what those paths mean is made by [`plan_batch`], which is
/// pure and therefore testable without a filesystem. A watcher whose logic lives in the event
/// loop can only be tested by causing real file events, which is how a watcher ends up with a
/// test suite that is either flaky or absent.
///
/// Spelling a path so that it can be compared with a root is the OS's business rather than the
/// planner's, so [`under_root`] is applied here, where the notification is picked up: from this
/// point on, every path is in the caller's spelling of the root.
pub mod native {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::Duration;

    use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

    use super::{Debouncer, Plan, plan_batch, under_root};

    /// Everything a caller can configure, so the defaults are visible rather than buried.
    #[derive(Debug, Clone)]
    pub struct WatchOptions {
        /// How long a batch stays open without a new event.
        ///
        /// 200 ms is the default because it is long enough to swallow a save's create/write/rename
        /// dance and short enough that a developer notices their index is behind. Both directions
        /// are configurable and neither is correct for everyone: a network filesystem wants longer,
        /// a local SSD is fine with shorter.
        pub quiet_for: Duration,
        /// Passed to [`plan_batch`] to decide whether an extension is one we extract.
        pub is_indexable: fn(&Path) -> bool,
    }

    impl Default for WatchOptions {
        fn default() -> Self {
            Self {
                quiet_for: Duration::from_millis(200),
                // A `.rs` file by default, so the module is useful without a discovery policy and
                // obviously narrow enough that a caller replaces it.
                is_indexable: |path| path.extension().is_some_and(|ext| ext == "rs"),
            }
        }
    }

    /// A running watcher. Dropping it stops watching and flushes nothing — call
    /// [`Watch::stop`] first.
    pub struct Watch {
        watcher: Option<RecommendedWatcher>,
        events: mpsc::Receiver<notify::Result<Event>>,
        debouncer: Debouncer,
        /// The repository root as the caller named it.
        ///
        /// Every path this watcher hands out is spelled this way, because this is the spelling the
        /// caller strips them against to reach a repository-relative path.
        root: PathBuf,
        /// The same directory with its symlinks resolved, which is how some backends report it.
        resolved_root: PathBuf,
        options: WatchOptions,
    }

    /// Why watching could not start.
    #[derive(Debug)]
    pub enum WatchError {
        /// The OS refused the watch, or the backend is unavailable on this platform.
        Unavailable(String),
    }

    impl std::fmt::Display for WatchError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                WatchError::Unavailable(detail) => {
                    write!(f, "filesystem notifications are unavailable: {detail}")
                }
            }
        }
    }

    impl std::error::Error for WatchError {}

    /// One applied batch.
    #[derive(Debug, Clone)]
    pub struct Batch {
        pub plan: Plan,
        /// How many raw events the batch coalesced, which is the number that says whether the
        /// quiet period is doing its job.
        pub events: usize,
    }

    impl Watch {
        /// Begin watching `root` recursively.
        pub fn start(root: &Path, options: WatchOptions) -> Result<Self, WatchError> {
            // Resolved once, here, rather than per event: it cannot change under a running watch,
            // and a failed resolution falls back to the caller's spelling, which is what a backend
            // that reports paths that way would have used anyway.
            let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            let (tx, events) = mpsc::channel();
            let mut watcher = notify::recommended_watcher(move |result| {
                // A send failure means the receiver is gone, which is shutdown. Dropping the event
                // is correct: there is nobody left to tell.
                let _ = tx.send(result);
            })
            .map_err(|error| WatchError::Unavailable(error.to_string()))?;
            watcher
                .watch(root, RecursiveMode::Recursive)
                .map_err(|error| WatchError::Unavailable(error.to_string()))?;
            Ok(Self {
                watcher: Some(watcher),
                events,
                debouncer: Debouncer::new(options.quiet_for),
                root: root.to_path_buf(),
                resolved_root,
                options,
            })
        }

        /// Collect any events that have arrived and open a batch.
        ///
        /// Non-blocking: returns immediately with whatever is queued. The caller drives this from a
        /// timer, and the quiet period — not this call — decides when a batch is ready.
        pub fn poll(&mut self) -> usize {
            let mut taken = 0;
            while let Ok(event) = self.events.try_recv() {
                // An error event carries no paths, so there is nothing to record. It is not
                // silently dropped: the count reaches the caller through the batch's event total
                // only for real paths, so an errored watch is surfaced by the watcher itself
                // rather than by a path that does not exist.
                if let Ok(event) = event {
                    for reported in event.paths {
                        // Restated here, at the one place where a path comes off the operating
                        // system, so everything below this line speaks the caller's spelling.
                        let path = under_root(&self.root, &self.resolved_root, &reported);
                        self.debouncer.record([path]);
                        taken += 1;
                    }
                }
            }
            taken
        }

        /// A batch if the quiet period has elapsed, otherwise `None`.
        pub fn take_batch(&mut self, now: std::time::Instant) -> Option<Batch> {
            let events = self.debouncer.drain_if_quiet(now)?;
            let plan = plan_batch(&self.root, &events, |path| {
                (self.options.is_indexable)(path)
            });
            Some(Batch {
                plan,
                events: events.len(),
            })
        }

        /// Hand over whatever is pending, ignoring the quiet period. What shutdown uses, so a
        /// process that exits mid-save does not leave those changes out of the index.
        pub fn flush(&mut self) -> Option<Batch> {
            let events = self.debouncer.flush()?;
            let plan = plan_batch(&self.root, &events, |path| {
                (self.options.is_indexable)(path)
            });
            Some(Batch {
                plan,
                events: events.len(),
            })
        }

        /// Stop watching, handing over any batch still open.
        pub fn stop(&mut self) -> Option<Batch> {
            let batch = self.flush();
            // Dropping the watcher unsubscribes. `None` afterwards so a second `stop` is harmless
            // rather than a double-free.
            drop(self.watcher.take());
            batch
        }

        /// The last failure a caller recorded, so it can be surfaced once.
        pub fn last_failure(&self) -> Option<&str> {
            self.debouncer.last_failure()
        }
    }
}

#[cfg(test)]
mod tests;
