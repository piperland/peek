//! `peek watch`: the watcher loop.
//!
//! # The split that makes this testable
//!
//! [`Session`] is the whole of the decision-making: given a [`Batch`], apply it and record what
//! happened. It has no threads, no filesystem watcher, and no clock, so a test drives it with
//! synthetic batches and asserts on what it accumulated. The loop that drives it is twenty lines
//! of glue over [`peek_core::watch::native::Watch`], and it is the only part that cannot be tested
//! without a real filesystem.
//!
//! # Shutdown, and the one thing this build cannot do
//!
//! On the way out the loop calls `stop()` and applies whatever batch that hands over, so a clean
//! exit does not leave an open batch out of the index. Two triggers reach it:
//!
//! * **the watched directory has gone**, which is the one shutdown this build can detect on its
//!   own; and
//! * **the age bound**, which closes a batch that would otherwise wait forever.
//!
//! **Ctrl-C is not one of them.** A process killed by a signal runs no destructor, so the final
//! flush does not happen and a batch open at that instant is not in the index. Detecting a signal
//! needs a dependency this crate does not have, and rather than take one silently the command says
//! so, in `--help` and in the `could not:` block of every run. It is a real gap and a named one.
//!
//! # The age bound
//!
//! A continuous writer produces events faster than the quiet period elapses, so without a bound
//! the batch never closes and the index falls arbitrarily behind while the watcher looks healthy.
//! `max_batch_ms` closes it anyway. The age is measured from the first event *this process*
//! observed, so the true age of a batch can exceed the bound by at most one poll interval —
//! stated because a bound that is quietly approximate is not a bound.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use peek_core::discover::DiscoveryOptions;
use peek_core::indexer;
use peek_core::watch::native::{Batch, Watch, WatchOptions};

use crate::answer::{Answer, DecisionAnswer, SkippedFileAnswer, WatchAnswer};
use crate::commands::{Outcome, need};
use crate::exit::{Failure, Refusal, Status, kind};
use crate::paths::{self, Location};
use crate::progress::Progress;

/// How many declined paths and refused files to name, per reason.
///
/// A **display bound, not a threshold**, and the reason it is stated: a watcher over a real
/// repository declines thousands of files over a session, and printing every one would bury the
/// batch it just applied. The count beside the sample is the complete number, so nothing is lost —
/// only the tail of a long list is not itemised.
pub const SAMPLE: usize = 5;

/// The most batch failures a session keeps.
///
/// A **memory bound, not a threshold**: a watcher whose every batch fails over a long session
/// would otherwise grow its failure list without limit, and the exit code quotes only the first.
/// That the list is capped is stated in the answer rather than left to be discovered.
pub const MAX_KEPT_FAILURES: usize = 32;

/// The statement every watch answer carries about the one shutdown this build cannot perform.
///
/// A named constant rather than a string written at the call site, so a test can assert that the
/// gap is still stated and a reworded message cannot quietly drop it. The words that matter are
/// "no signal handler" and "Ctrl-C": a reader who is told merely that shutdown is "best effort"
/// cannot tell whether their changes are safe.
pub const SIGNALS_NOT_CAUGHT: &str = "this build has no signal handler, so Ctrl-C ends the \
     process without the shutdown path: a batch open at that moment is not in the index. The clean \
     shutdowns are the watched directory disappearing and the age bound closing an open batch";

/// How often the loop looks for a batch that is ready.
///
/// Pacing, not a quality threshold: the loop's work is bounded by the watcher's own event stream,
/// and this only decides how long a ready batch waits before it is noticed. Derived from the quiet
/// period rather than chosen, so one number governs both and there is no second knob to argue
/// about. Clamped at both ends so a very small or very large quiet period still yields a loop that
/// neither spins nor sleeps through a batch.
fn tick(quiet_for: Duration) -> Duration {
    let derived = quiet_for / 8;
    derived.clamp(Duration::from_millis(1), Duration::from_millis(25))
}

/// Run the watcher until the watched directory disappears.
pub fn run(
    location: Option<&Location>,
    command: &'static str,
    quiet_for_ms: u64,
    max_batch_ms: u64,
    progress: &mut dyn Progress,
) -> Result<Outcome, Failure> {
    let location = need(location, command)?;
    if !location.index_existed {
        return Err(Failure::refused(
            command,
            Refusal::new(
                kind::NO_INDEX,
                format!(
                    "there is no index for this repository: {} does not exist, so there is \
                     nothing to keep up to date. `peek index` builds one",
                    location.index_text()
                ),
            ),
        ));
    }
    let quiet_for = Duration::from_millis(quiet_for_ms);
    let max_batch = Duration::from_millis(max_batch_ms);
    let options = WatchOptions {
        quiet_for,
        is_indexable: crate::args::is_indexable,
    };
    let mut watcher = Watch::start(&location.root, options).map_err(|error| {
        Failure::failed(
            command,
            Refusal::new(
                kind::WATCH_UNAVAILABLE,
                format!("watching {} could not start: {error}", location.root_text()),
            ),
        )
    })?;
    progress.note(format!(
        "watching {} with a {quiet_for_ms} ms quiet period and a {max_batch_ms} ms batch bound",
        location.root_text()
    ));

    let mut store = paths::open_store(location, command)?;
    let discovery = DiscoveryOptions::default();
    let mut session = Session::new();
    let mut batch_opened: Option<Instant> = None;

    loop {
        if watcher.poll() > 0 && batch_opened.is_none() {
            batch_opened = Some(Instant::now());
        }
        let now = Instant::now();
        let aged_out = batch_opened.is_some_and(|opened| now.duration_since(opened) >= max_batch);
        let batch = match (aged_out, watcher.take_batch(now)) {
            (_, Some(batch)) => Some(batch),
            (true, None) => {
                progress.note(
                    "a batch has been open for the whole age bound; closing it so the index does \
                     not fall arbitrarily behind"
                        .to_owned(),
                );
                watcher.flush()
            }
            (false, None) => None,
        };
        if let Some(batch) = batch {
            batch_opened = None;
            let applied = session.apply(&mut store, &location.root, batch, &discovery);
            progress.note(applied.describe());
        }
        // The one shutdown this build can detect on its own. A directory that has been removed is
        // unambiguous, and it is the point at which a watch has nothing left to watch.
        if !location.root.is_dir() {
            progress.note(format!(
                "{} is gone; shutting down and applying whatever is still open",
                location.root_text()
            ));
            break;
        }
        std::thread::sleep(tick(quiet_for));
    }

    // Shutdown: hand over whatever batch is still open, and apply it. A batch still open at exit is
    // a batch whose changes are not in the index, so this is the difference between a clean exit
    // and a stale one.
    let final_batch = watcher.stop();
    if let Some(batch) = &final_batch {
        let applied = session.apply(&mut store, &location.root, batch.clone(), &discovery);
        progress.note(applied.describe());
    }

    let mut did_not = vec![
        SIGNALS_NOT_CAUGHT.to_owned(),
        "this did not diff anything: each batch re-extracts the files that changed, and the \
         resolver is scoped to those files and to the edges that pointed into them"
            .to_owned(),
    ];
    if session.outside_root > 0 {
        did_not.push(format!(
            "{} path(s) arrived from outside the repository, which means the watcher is watching \
             something it was not asked to watch. They were counted and not touched",
            session.outside_root
        ));
    }
    if session.ignored_sample.len() < session.ignored_by_reason.values().sum::<u64>() as usize {
        did_not.push(format!(
            "{} of the declined paths are itemised above and the rest are counted only; a watcher \
             over a real repository declines thousands of files and naming every one would bury \
             the batch it just applied",
            session.ignored_sample.len()
        ));
    }
    if session.failures.is_empty() {
        did_not.push(
            "this did not verify that the index matches the working tree at the end of the session; \
             `peek status` reports the counts and `peek doctor` the consistency"
                .to_owned(),
        );
    }

    let answer = WatchAnswer {
        quiet_for_ms,
        max_batch_ms,
        batches_applied: session.batches,
        events_seen: session.events,
        paths_reindexed: session.paths_reindexed,
        ignored_by_reason: session.ignored_by_reason.clone(),
        ignored_sample: session.ignored_sample(),
        outside_root: session.outside_root,
        skipped_by_reason: session.skipped_by_reason.clone(),
        skipped_sample: session.skipped_sample(),
        first_generation: session.first_generation,
        last_generation: session.last_generation,
        failures: session.failures.clone(),
        notes: session.notes.clone(),
        did_not,
    };

    // A batch that failed is an index that is behind, and a watcher that exits zero having dropped
    // a refresh reports itself as current. That is the failure the debouncer's own `last_failure`
    // exists to prevent, so the exit code follows the same rule.
    if session.failures.is_empty() {
        Ok(Outcome::ok(Answer::Watch(answer)))
    } else {
        Ok(Outcome::limited(
            Status::Refused,
            Refusal::new(
                kind::WATCH_INCOMPLETE,
                format!(
                    "{} of {} batch(es) failed to apply, so the index is behind the working tree: \
                     the first was {}",
                    session.failures.len(),
                    session.batches,
                    session.failures[0]
                ),
            ),
            Answer::Watch(answer),
        ))
    }
}

/// What one applied batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Paths the plan asked to re-index.
    pub reindexed: usize,
    /// Files the indexer read and extracted.
    pub indexed: u64,
    /// Files the indexer refused, with the reasons.
    pub skipped: Vec<String>,
    /// Whether the refresh landed.
    pub ok: bool,
}

impl Applied {
    /// One line of narration, for the progress sink.
    #[must_use]
    pub fn describe(&self) -> String {
        let outcome = if self.ok { "applied" } else { "FAILED" };
        let mut line = format!(
            "batch {outcome}: {} path(s), {} file(s) indexed",
            self.reindexed, self.indexed
        );
        if !self.skipped.is_empty() {
            let mut reasons: BTreeMap<&str, u64> = BTreeMap::new();
            for reason in &self.skipped {
                let key = reason
                    .split_once(": ")
                    .map_or(reason.as_str(), |(_, rest)| rest);
                *reasons.entry(key).or_default() += 1;
            }
            let grouped: Vec<String> = reasons
                .iter()
                .map(|(reason, count)| format!("{count} {reason}"))
                .collect();
            line.push_str(&format!(", not indexed: {}", grouped.join(", ")));
        }
        line
    }
}

/// The accumulating half of a watch session.
///
/// Owned separately from the loop so a test can drive it with synthetic batches and assert on
/// every number, which is the only way a watcher's *decisions* get tested at all.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// Batches applied.
    pub batches: u64,
    /// Raw events coalesced into them.
    pub events: u64,
    /// Distinct paths re-indexed.
    pub paths_reindexed: u64,
    /// Declined paths, grouped by the reason the plan gave.
    pub ignored_by_reason: BTreeMap<String, u64>,
    /// Paths that arrived from outside the repository.
    pub outside_root: u64,
    /// Files the indexer refused, grouped by reason.
    pub skipped_by_reason: BTreeMap<String, u64>,
    /// Every batch that failed, with why.
    pub failures: Vec<String>,
    /// Statements the session owes its reader.
    pub notes: Vec<String>,
    /// The generation before the first batch, if there was one.
    pub first_generation: Option<u64>,
    /// The generation after the last.
    pub last_generation: Option<u64>,
    ignored: Vec<DecisionAnswer>,
    skipped: Vec<SkippedFileAnswer>,
}

/// The most declined or refused paths a session retains for its sample.
///
/// A **memory bound, not a threshold**: a watcher over a large repository declines thousands of
/// files over a session, and retaining every one of them would grow without limit. The counts in
/// [`Session::ignored_by_reason`] and [`Session::skipped_by_reason`] are complete; only the
/// itemised sample is bounded, and the answer says so.
pub const MAX_SAMPLE_ENTRIES: usize = 64;

impl Session {
    /// An empty session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// A sample of the declined paths, at most [`SAMPLE`] per reason.
    #[must_use]
    pub fn ignored_sample(&self) -> Vec<DecisionAnswer> {
        sample_by(&self.ignored, |entry| &entry.reason)
    }

    /// A sample of the refused files, at most [`SAMPLE`] per reason.
    #[must_use]
    pub fn skipped_sample(&self) -> Vec<SkippedFileAnswer> {
        sample_by(&self.skipped, |entry| &entry.reason)
    }

    /// Apply one batch.
    ///
    /// A refresh that fails is **recorded and survived**, never fatal: a watcher that stopped on
    /// the first error would leave a stale index and report itself as stopped, which is the same
    /// dishonesty as silently dropping the failure. The exit code is raised at the end instead.
    pub fn apply(
        &mut self,
        store: &mut peek_core::store::Store,
        root: &Path,
        batch: Batch,
        options: &DiscoveryOptions,
    ) -> Applied {
        self.batches += 1;
        self.events += batch.events as u64;
        self.paths_reindexed += batch.plan.reindex.len() as u64;
        self.outside_root += batch.plan.outside_root.len() as u64;
        for decision in &batch.plan.ignored {
            *self
                .ignored_by_reason
                .entry(decision.reason.to_owned())
                .or_default() += 1;
            if self.ignored.len() < MAX_SAMPLE_ENTRIES {
                self.ignored.push(DecisionAnswer {
                    path: decision.path.display().to_string(),
                    reason: decision.reason.to_owned(),
                });
            }
        }

        // An empty plan is not a failure and does not commit: a generation that advances for a
        // batch that changed nothing makes the counter useless as a change signal.
        if batch.plan.is_empty() {
            return Applied {
                reindexed: 0,
                indexed: 0,
                skipped: Vec::new(),
                ok: true,
            };
        }

        if self.first_generation.is_none() {
            self.first_generation = Some(store.generation());
        }
        match indexer::refresh(store, root, &batch.plan.reindex, options) {
            Ok(outcome) => {
                self.last_generation = Some(outcome.report().generation);
                let skipped: Vec<String> = outcome
                    .skipped
                    .iter()
                    .map(|entry| format!("{}: {}", entry.path, entry.reason))
                    .collect();
                for entry in &outcome.skipped {
                    *self
                        .skipped_by_reason
                        .entry(entry.reason.to_string())
                        .or_default() += 1;
                    if self.skipped.len() < MAX_SAMPLE_ENTRIES {
                        self.skipped.push(SkippedFileAnswer::from(entry));
                    }
                }
                if !outcome.skipped.is_empty() {
                    self.notes.push(format!(
                        "a batch considered {} file(s) and did not index {} of them; each reason is \
                         counted in `skipped_by_reason`",
                        outcome.report().files_indexed,
                        outcome.skipped.len()
                    ));
                }
                Applied {
                    reindexed: batch.plan.reindex.len(),
                    indexed: outcome.report().files_indexed,
                    skipped,
                    ok: true,
                }
            }
            Err(error) => {
                // Bounded: a watcher whose every batch fails over a long session would otherwise
                // grow this list without limit, and the exit code quotes only the first failure.
                // That the list is capped is stated in the answer rather than left to be discovered.
                if self.failures.len() < MAX_KEPT_FAILURES {
                    self.failures.push(error.to_string());
                } else {
                    self.notes.push(format!(
                        "more than {MAX_KEPT_FAILURES} batches failed; the earliest are reported and \
                         the rest are counted here"
                    ));
                }
                self.last_generation = Some(store.generation());
                Applied {
                    reindexed: batch.plan.reindex.len(),
                    indexed: 0,
                    skipped: Vec::new(),
                    ok: false,
                }
            }
        }
    }
}

/// The first [`SAMPLE`] entries per distinct key, in order.
fn sample_by<T, F>(entries: &[T], key: F) -> Vec<T>
where
    T: Clone,
    F: Fn(&T) -> &str,
{
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for entry in entries {
        let key = key(entry);
        let count = counts.entry(key).or_default();
        if *count < SAMPLE {
            *count += 1;
            out.push(entry.clone());
        }
    }
    out
}
