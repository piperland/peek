//! `watch_start` and `watch_stop`: keeping the index current without blocking a tool call.
//!
//! # The shape of the problem
//!
//! A watcher is a thread that never finishes. A tool call is a request that must produce a reply.
//! Putting a watcher loop inside a handler means the handler never returns and the client hangs
//! until it times out. So the watch lives on its own thread and `watch_start` returns as soon as
//! there is something honest to say about it.
//!
//! # What "immediately" means here, precisely
//!
//! `watch_start` waits for exactly one thing: the operating system confirming that it registered a
//! recursive watch on the repository. That is a syscall, and it is the only step between the call
//! and a watcher that exists. It is bounded by `ready_timeout_ms` so that a platform where the call
//! does not return cannot hang the tool call forever — and if the bound is reached, the response
//! says the watch did not start and why, rather than reporting a watch that is not there. It does
//! **not** wait for a file event, a debounce window or a refresh; those are reported by later calls.
//!
//! # Why the writer lives on the thread
//!
//! SQLite admits one writer at a time. The watcher thread opens its own store and holds it for its
//! whole life, which is what makes `index` refuse while a watch runs instead of contending for the
//! lock. The session's reader is a separate connection, so queries keep working while a watch
//! commits — which is also why `index_status` reports two generations.
//!
//! # How a refresh reaches the caller, and what is lost
//!
//! There is exactly one channel: a [`SharedState`] the thread writes and the session reads, holding
//! **the most recent** report and **the most recent** failure. A second channel beside it would be
//! two descriptions of the same fact and would drift; this is one slot, read between calls.
//!
//! What that costs is intermediate reports: a refresh applied while a caller was not looking has
//! its report replaced by the next one. So each replacement increments `reports_superseded`, and
//! `index_status` and `watch_stop` report it. A superseded report is a *reporting* loss and not a
//! state loss — the batch was applied to the store before its report was ever written — and the
//! difference is stated wherever the counter appears. An unreported supersession would be a silent
//! truncation wearing a counter, which is the same defect as a silently truncated answer.
//!
//! # Shutdown
//!
//! `watch_stop` sets a flag, then **joins** the thread. The join is what makes stopping honest: the
//! thread flushes its open batch on the way out, so a change made in the moment before the stop is
//! in the index rather than pending in a thread that was killed. Dropping the session does the same
//! thing, so a client that disconnects without calling `watch_stop` does not leave a thread holding
//! the writer in a process that is on its way out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use peek_core::discover::DiscoveryOptions;
use peek_core::indexer;
use peek_core::model::Language;
use peek_core::store::Store;
use peek_core::watch::native::{Watch, WatchOptions};

use serde::Serialize;
use serde_json::Value;

use crate::outcome::{Outcome, ToolError};
use crate::params::Args;
use crate::session::{RunningWatch, Session};
use crate::tools::ToolAnswer;
use crate::tools::index::IndexReportView;

/// How long a batch stays open with no new event, when the caller does not say.
///
/// The engine's own default, restated here so the number appears in one place a reader can check
/// against `peek_core::watch::native::WatchOptions`.
pub const DEFAULT_QUIET_FOR_MS: u64 = 200;

/// How long to wait for the operating system to confirm the watch, when the caller does not say.
///
/// A bound on the *call*, not on the watch, and **not a claim about how long registration takes** —
/// I have not measured it. It exists so that a platform where the registration call does not return
/// cannot hang a tool call forever, and it is a parameter so a caller on a slow or network-mounted
/// filesystem can raise it. When it is reached the response says the watch did not start.
pub const DEFAULT_READY_TIMEOUT_MS: u64 = 10_000;

/// How often the watcher thread wakes to look for events.
///
/// Not a threshold about anything: it is how long the thread sleeps between polls of a
/// non-blocking queue, so it is the granularity at which a change becomes a refresh.
const POLL_INTERVAL_MS: u64 = 50;

/// Start watching, and return immediately.
pub fn start(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    let args = Args::new(
        "watch_start",
        arguments,
        crate::tool::hints_for("watch_start"),
    )?;
    let quiet_for_ms = args
        .optional_u64("quiet_for_ms")?
        .unwrap_or(DEFAULT_QUIET_FOR_MS);
    let ready_timeout_ms = args
        .optional_u64("ready_timeout_ms")?
        .unwrap_or(DEFAULT_READY_TIMEOUT_MS);
    args.finish(&["quiet_for_ms", "ready_timeout_ms"])?;

    let quiet_for = Duration::from_millis(quiet_for_ms);
    let ready_timeout = Duration::from_millis(ready_timeout_ms);
    let started = session.start_watch(quiet_for, ready_timeout)?;
    let status = session
        .watch_state()
        .ok_or_else(|| ToolError::failed("the watch started and then vanished"))?;

    let text = format!(
        "watch {} is running on {}.\n\
         it re-indexes {} language(s) this build can extract, coalescing a burst of changes over \
         {} ms into one refresh.\n\
         the index it maintains is at {}.\n\
         call `watch_stop` with {{\"watch_id\": {}}} to stop it; a stopped watch applies whatever \
         is still pending before it exits.",
        started.id,
        started.root,
        started.watching.len(),
        started.quiet_for_ms,
        started.index_path,
        started.id
    );
    let body = serde_json::json!({
        "outcome": Outcome::Ok.as_str(),
        "reason": Value::Null,
        "advice": Value::Null,
        "candidates": [],
        "watch_id": started.id,
        "root": started.root,
        "index_path": started.index_path,
        "quiet_for_ms": started.quiet_for_ms,
        "ready_timeout_ms": started.ready_timeout_ms,
        "watching": started.watching,
        // The stop call, spelled out. A caller that reads this knows how to end the watch without
        // reconstructing the call, which is the difference between a bounded tool and one that
        // silently keeps a process alive.
        "stop_with": { "tool": "watch_stop", "arguments": { "watch_id": started.id } },
        "state": status,
    });
    Ok(ToolAnswer::new(text, body))
}

/// Stop the watch, apply what is pending, and report what it did.
pub fn stop(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    let args = Args::new(
        "watch_stop",
        arguments,
        crate::tool::hints_for("watch_stop"),
    )?;
    let watch_id = args.optional_u64("watch_id")?;
    args.finish(&["watch_id"])?;

    let stopped = session.stop_watch(watch_id)?;
    let final_refresh = match &stopped.final_refresh {
        Some(report) => report.render(),
        None => "none; no change had been applied".to_owned(),
    };
    let text = format!(
        "watch {} stopped. {} refresh(es) applied, {} failed, {} report(s) superseded before \
         they were read.\nfinal refresh: {final_refresh}",
        stopped.id, stopped.applied, stopped.failed, stopped.reports_superseded
    );
    let body = serde_json::json!({
        "outcome": if stopped.stopped_cleanly { Outcome::Ok.as_str() } else { Outcome::Failed.as_str() },
        "reason": stopped.reason.clone().map(Value::String).unwrap_or(Value::Null),
        "advice": if stopped.stopped_cleanly {
            Value::Null
        } else {
            Value::String("run `doctor`; a watcher that did not stop cleanly may have left the index behind an uncommitted batch")
        },
        "candidates": [],
        "watch_id": stopped.id,
        "applied": stopped.applied,
        "failed": stopped.failed,
        "reports_superseded": stopped.reports_superseded,
        "stopped_cleanly": stopped.stopped_cleanly,
        "final_refresh": stopped.final_refresh,
    });
    Ok(ToolAnswer::new(text, body))
}

/// What `watch_start` reports about the watch it started.
#[derive(Debug, Clone, Serialize)]
pub struct StartedWatch {
    /// The id that stops it.
    pub id: u64,
    /// The repository being watched.
    pub root: String,
    /// The coalescing window, in milliseconds.
    pub quiet_for_ms: u64,
    /// The bound this call waited for the operating system, in milliseconds.
    pub ready_timeout_ms: u64,
    /// Where the index this watch maintains lives.
    pub index_path: String,
    /// The languages this build can extract, and therefore the files it will re-index.
    pub watching: Vec<&'static str>,
}

/// The state of a running watch, as `watch_start` and `index_status` report it.
#[derive(Debug, Clone, Serialize)]
pub struct WatchStatusView {
    pub id: u64,
    /// False once the thread has exited, which is how a watcher that died is visible rather than
    /// being reported as running until the process ends.
    pub running: bool,
    /// Refreshes applied to the store.
    pub applied: u64,
    /// Refreshes that could not be applied. Non-zero means the index is behind the filesystem.
    pub failed: u64,
    /// Applied refreshes whose report was replaced by a later one before a caller read it. A
    /// reporting loss, not a state loss.
    pub reports_superseded: u64,
    /// The most recent refresh, or `null` when none has been applied.
    pub last_refresh: Option<IndexReportView>,
    /// The most recent failure, or `null`.
    pub last_error: Option<String>,
    pub root: String,
    pub quiet_for_ms: u64,
    /// The languages this watch re-indexes.
    pub watching: Vec<&'static str>,
}

/// What `watch_stop` reports.
#[derive(Debug, Clone, Serialize)]
pub struct StoppedWatch {
    pub id: u64,
    pub applied: u64,
    pub failed: u64,
    /// Applied refreshes whose report was replaced before it was read.
    pub reports_superseded: u64,
    /// The last refresh the thread produced, or `null` when it never applied one.
    pub final_refresh: Option<IndexReportView>,
    /// False when the thread ended abnormally. A watcher that panics is an index that claims to be
    /// current when it may not be, and it is reported as a failure rather than as a clean stop.
    pub stopped_cleanly: bool,
    pub reason: Option<String>,
}

/// The languages this build has extraction rules for.
///
/// Derived from the extractor's own registry rather than from a list kept here, so the answer to
/// "what will this watch re-index" cannot be a second vocabulary that has fallen behind. Sorted,
/// because the registry's order is an implementation detail and a response that varied with it
/// would break the determinism claim.
#[must_use]
pub fn watched_languages() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = peek_core::extract::registry::all()
        .iter()
        .map(|spec| spec.language.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// What the watcher thread shares with the session.
///
/// One slot, not a queue. See the module documentation: a queue would need a bound, and a bound
/// means a dropped notification, and a dropped notification nobody counted is the defect this
/// crate exists to remove. One slot plus a count of supersessions says the same thing without
/// needing a bound at all.
#[derive(Debug)]
pub struct SharedState {
    inner: Mutex<SharedInner>,
    root: PathBuf,
    quiet_for_ms: u64,
}

#[derive(Debug, Default)]
struct SharedInner {
    last_refresh: Option<IndexReportView>,
    last_error: Option<String>,
}

impl SharedState {
    fn new(root: PathBuf, quiet_for_ms: u64) -> Self {
        Self {
            inner: Mutex::new(SharedInner::default()),
            root,
            quiet_for_ms,
        }
    }

    /// The most recent refresh.
    #[must_use]
    pub fn last_refresh(&self) -> Option<IndexReportView> {
        lock(&self.inner).last_refresh.clone()
    }

    /// The most recent failure.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        lock(&self.inner).last_error.clone()
    }

    /// The repository being watched.
    #[must_use]
    pub fn root(&self) -> String {
        self.root.display().to_string()
    }

    /// The coalescing window in force, in milliseconds.
    #[must_use]
    pub fn quiet_for_ms(&self) -> u64 {
        self.quiet_for_ms
    }

    /// Take the most recent refresh out of the slot, so a caller that asks twice does not read it
    /// twice.
    pub fn take_last_refresh(&self) -> Option<IndexReportView> {
        lock(&self.inner).last_refresh.take()
    }

    /// Record an applied refresh, reporting whether it replaced an unread one.
    fn record_refresh(&self, report: IndexReportView) -> bool {
        let mut inner = lock(&self.inner);
        let replaced = inner.last_refresh.is_some();
        inner.last_refresh = Some(report);
        replaced
    }

    /// Record a failure.
    fn record_error(&self, error: String) {
        lock(&self.inner).last_error = Some(error);
    }
}

/// Lock a mutex, poisoning included.
///
/// A poisoned mutex means the watcher thread panicked while holding it, which is a real event worth
/// surviving: propagating it to the *next* tool call would turn one thread's failure into a dead
/// server. What is behind it is two `Option`s that are only ever replaced wholesale, so a stale
/// read cannot produce a half-written state.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the watcher thread counts.
///
/// Public because [`crate::session::RunningWatch`] holds it and reports from it, and the counters
/// are the only way a caller learns what a watch did. The fields are private and read through
/// methods, so a caller cannot reset one by accident and a counter cannot be half-read.
#[derive(Debug, Default)]
pub struct Counters {
    applied: AtomicU64,
    failed: AtomicU64,
    superseded: AtomicU64,
}

impl Counters {
    /// Refreshes applied to the store.
    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }

    /// Refreshes that could not be applied. Non-zero means the index is behind the filesystem.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Applied refreshes whose report was replaced by a later one before a caller read it.
    pub fn superseded(&self) -> u64 {
        self.superseded.load(Ordering::Relaxed)
    }
}

/// Spawn the watcher thread, and wait for the operating system to confirm the watch.
///
/// Returns only once a watch exists or has failed. A failure inside the thread is sent back through
/// the readiness channel rather than logged and forgotten, so `watch_start` can report it — a tool
/// that reports a watch which is not there is exactly the defect this crate exists to avoid.
pub(crate) fn spawn(
    id: u64,
    root: PathBuf,
    quiet_for: Duration,
    ready_timeout: Duration,
) -> Result<RunningWatch, ToolError> {
    let stop = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(Counters::default());
    let state = Arc::new(SharedState::new(
        root.clone(),
        u64::try_from(quiet_for.as_millis()).unwrap_or(u64::MAX),
    ));
    let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);

    // Named `handles` rather than `thread` so it does not sit next to the `std::thread` module it
    // borrows names from. The two would resolve correctly today, and the next reader would have to
    // work out why.
    let handles = ThreadHandles {
        stop: Arc::clone(&stop),
        counters: Arc::clone(&counters),
        state: Arc::clone(&state),
    };
    let join = thread::Builder::new()
        .name(format!("peek-watch-{id}"))
        .spawn(move || handles.run(root, quiet_for, &ready_tx))
        .map_err(|error| {
            ToolError::failed(format!(
                "the operating system would not start a thread to watch this repository: {error}"
            ))
        })?;

    match ready_rx.recv_timeout(ready_timeout) {
        Ok(Ok(())) => Ok(RunningWatch::new(id, stop, join, state, counters)),
        Ok(Err(reason)) => {
            // The thread returns after sending, so the join is immediate.
            let _ = join.join();
            Err(ToolError::refused(
                format!("watch {id} could not be started: {reason}"),
                "check that the repository exists and is readable; filesystem notifications are \
                 unavailable on some filesystems and in some containers",
            ))
        }
        Err(error) => {
            // The thread has not confirmed. Ask it to stop and detach rather than joining: joining
            // would block for as long as the operating system call takes, which is the very thing
            // the timeout exists to bound. The thread checks the flag immediately after the call
            // returns, so it exits as soon as it can and releases the writer. If the call never
            // returns, only a process-level timeout helps, and the refusal says so.
            stop.store(true, Ordering::SeqCst);
            Err(ToolError::refused(
                format!(
                    "watch {id} did not confirm within {} ms ({error})",
                    ready_timeout.as_millis()
                ),
                "no watcher is running for this session; the thread that was starting it will exit \
                 as soon as the operating system call returns, and the store is released when it \
                 does. Raise `ready_timeout_ms` if this filesystem is slow to register",
            ))
        }
    }
}

/// The handles a spawned watcher thread needs.
struct ThreadHandles {
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    state: Arc<SharedState>,
}

impl ThreadHandles {
    /// The thread body: open the writer, register the watch, then loop until asked to stop.
    fn run(self, root: PathBuf, quiet_for: Duration, ready: &SyncSender<Result<(), String>>) {
        let mut store = match indexer::open_store(&root) {
            Ok(store) => store,
            Err(error) => {
                let _ = ready.send(Err(format!(
                    "the index for {} could not be opened for writing: {error}",
                    root.display()
                )));
                return;
            }
        };
        let mut watcher = match Watch::start(&root, watch_options(quiet_for)) {
            Ok(watcher) => watcher,
            Err(error) => {
                let _ = ready.send(Err(error.to_string()));
                return;
            }
        };
        // Only now is a watch genuinely running, so this is the only point at which readiness may
        // be reported. Reporting earlier would be reporting a watch that does not exist.
        if self.stop.load(Ordering::SeqCst) || ready.send(Ok(())).is_err() {
            // Either the caller gave up while the call above was running, or it has gone away.
            // Nothing left to watch for.
            return;
        }

        let poll = Duration::from_millis(POLL_INTERVAL_MS);
        while !self.stop.load(Ordering::SeqCst) {
            watcher.poll();
            if let Some(batch) = watcher.take_batch(std::time::Instant::now()) {
                self.apply(&mut store, &root, &batch.plan.reindex);
            }
            thread::sleep(poll);
        }

        // The final flush. A batch open at shutdown is a batch whose changes are not in the index,
        // and that is the difference between a clean stop and a stale index.
        if let Some(batch) = watcher.stop() {
            self.apply(&mut store, &root, &batch.plan.reindex);
        }
        if let Some(reason) = watcher.last_failure() {
            self.state
                .record_error(format!("the watcher reported a failure: {reason}"));
        }
    }

    /// Apply one batch, and record what happened.
    fn apply(&self, store: &mut Store, root: &Path, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        match indexer::refresh(store, root, paths, &DiscoveryOptions::default()) {
            Ok(outcome) => {
                self.counters.applied.fetch_add(1, Ordering::Relaxed);
                if self.state.record_refresh(IndexReportView::of(&outcome)) {
                    self.counters.superseded.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(error) => {
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
                // A refresh that failed leaves the index behind the filesystem, and a caller who
                // is not told will read the stale index as current.
                self.state.record_error(format!("{error}"));
            }
        }
    }
}

/// Watch options that re-index every language this build can extract.
///
/// The engine's own `WatchOptions::default` accepts `.rs` and nothing else, which is documented as
/// a placeholder. A watcher for this server has to use the extractor's registry, or it would
/// silently skip every file that is not Rust — a silent skip in a tool whose whole job is not
/// skipping things.
fn watch_options(quiet_for: Duration) -> WatchOptions {
    WatchOptions {
        quiet_for,
        is_indexable: |path: &std::path::Path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .and_then(Language::from_extension)
                .is_some_and(|language| peek_core::extract::registry::get(language).is_some())
        },
    }
}
