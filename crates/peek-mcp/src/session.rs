//! The engine session: one repository, one index, one optional watcher.
//!
//! # Why the store is opened lazily and only when it exists
//!
//! `Store::open` **creates** a missing index. A server that opened its store at startup would
//! therefore create an empty index in the cache directory merely by being launched, and — worse —
//! every query tool would then answer from an empty index and return an empty result that means
//! "this repository has never been indexed". That is the exact conflation the outcome taxonomy
//! exists to prevent.
//!
//! So [`Session::reader`] checks that the file is there first and refuses with
//! [`crate::Outcome::NotIndexed`] when it is not. An index is something the caller asked for with
//! `index`, not a side effect of connecting.
//!
//! # One reader, one writer
//!
//! The session holds a **reader**, opened on first use and kept. `index` and `watch_start` need a
//! **writer**, and they open their own short-lived one: `Store::apply_update` wants `&mut Store`,
//! and SQLite admits one writer at a time, so two of them in one process would contend rather than
//! cooperate. The watcher thread owns its writer for its whole life.
//!
//! The consequence worth stating is that a long-lived reader's cached generation goes stale the
//! moment a watcher commits. [`crate::tools::index::status`] therefore reports the generation the
//! handle was opened at *and* the generation the index currently records — the second read from
//! the database rather than from the handle — and says whether they differ, rather than quietly
//! serving a number that is no longer true.
//!
//! # Logging
//!
//! Every diagnostic goes through [`Log`], which the binary points at stderr. There is no path from
//! a tool handler to the process's stdout; see [`crate::writer`].

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use peek_core::store::{self, RepoId, Store};

use crate::outcome::ToolError;
use crate::tools::watch;
use crate::tools::watch::Counters;

/// Where diagnostics go.
pub trait Log {
    /// Record one line. Implementations must not write to stdout.
    fn note(&mut self, message: &str);
}

/// Writes each line to stderr, followed by a newline.
///
/// stderr and not stdout because stdout is the protocol channel: one line of diagnostics on the
/// wrong stream is a corrupted session. See [`crate::writer`].
#[derive(Debug, Default, Clone, Copy)]
pub struct StderrLog;

impl Log for StderrLog {
    fn note(&mut self, message: &str) {
        eprintln!("peek-mcp: {message}");
    }
}

/// Keeps every line in memory, behind a shared handle, for tests.
///
/// Cloneable so a test can hold one handle while the session holds another, which is what makes
/// "the server logged something" checkable from outside the crate. Public for the same reason the
/// claim needs to be testable from the integration tests rather than only from inside.
#[derive(Debug, Clone, Default)]
pub struct SharedLog {
    lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl SharedLog {
    /// A log with nothing in it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every line, in order.
    ///
    /// Cloned rather than borrowed: a test wants to assert on the lines, and holding the lock
    /// across an assertion is a way to deadlock a test against the server it is testing.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Whether anything has been logged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines().is_empty()
    }

    /// Whether any line contains `needle`. For a test that wants to know *what* was said without
    /// asserting on the whole transcript.
    #[must_use]
    pub fn mentions(&self, needle: &str) -> bool {
        self.lines().iter().any(|line| line.contains(needle))
    }
}

impl Log for SharedLog {
    fn note(&mut self, message: &str) {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(message.to_owned());
    }
}

/// The repository, the index, and whatever is watching it.
pub struct Session {
    root: PathBuf,
    repo: Option<RepoId>,
    /// Why the repository could not be identified, if it could not be. A query tool says this
    /// rather than reporting an unknown target, because "I cannot tell which repository this is"
    /// and "that symbol is not in the index" are different facts.
    identity_error: Option<String>,
    index_path: Option<PathBuf>,
    reader: Option<Store>,
    /// The generation the reader was opened at. Kept so a stale handle is visible rather than
    /// silently reported as current.
    opened_at_generation: u64,
    watch: Option<RunningWatch>,
    next_watch_id: u64,
    /// Request ids the client cancelled before they were dispatched.
    cancelled: BTreeSet<String>,
    log: Box<dyn Log>,
}

impl Session {
    /// Open a session for `root`.
    ///
    /// Never fails. A root that does not exist, a directory with no read permission and a path
    /// with no cache directory are all states a tool has to be able to *report*, and a session
    /// that refused to start would leave nowhere to report them.
    #[must_use]
    pub fn new(root: &Path, log: Box<dyn Log>) -> Self {
        let (repo, identity_error) = match RepoId::discover(root) {
            Ok(repo) => (Some(repo), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let index_path = repo
            .as_ref()
            .and_then(|repo| store::paths::index_path(repo).ok());
        let summary = index_path
            .as_ref()
            .map_or_else(|| "unresolved".to_owned(), |path| path.display().to_string());
        let mut session = Self {
            root: root.to_path_buf(),
            repo,
            identity_error,
            index_path,
            reader: None,
            opened_at_generation: 0,
            watch: None,
            next_watch_id: 1,
            cancelled: BTreeSet::new(),
            log,
        };
        session.log(&format!(
            "session open for {} (index: {summary})",
            root.display()
        ));
        session
    }

    /// The repository this session answers about.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the index file is, or why that could not be determined.
    #[must_use]
    pub fn index_path(&self) -> Option<&Path> {
        self.index_path.as_deref()
    }

    /// The repository identity, or why there is not one.
    pub fn identity(&self) -> Result<(&RepoId, &Path), ToolError> {
        match (&self.repo, &self.index_path) {
            (Some(repo), Some(path)) => Ok((repo, path.as_path())),
            (None, _) => Err(ToolError::failed(format!(
                "{} could not be identified as a repository: {}",
                self.root.display(),
                self.identity_error
                    .as_deref()
                    .unwrap_or("no reason was recorded")
            ))),
            _ => Err(ToolError::failed(format!(
                "no index location could be resolved for {}",
                self.root.display()
            ))),
        }
    }

    /// A read handle on the index, opening one if this is the first read.
    ///
    /// Refuses when the index does not exist rather than creating it. See the module
    /// documentation for why.
    pub fn reader(&mut self) -> Result<&Store, ToolError> {
        if self.reader.is_none() {
            let (repo, path) = self.identity()?;
            if !path.exists() {
                return Err(ToolError::not_indexed(&self.root));
            }
            let store = Store::open(path, repo).map_err(|error| {
                ToolError::failed(format!(
                    "the index at {} could not be opened: {error}",
                    path.display()
                ))
            })?;
            self.opened_at_generation = store.generation();
            self.reader = Some(store);
        }
        match &self.reader {
            Some(store) => Ok(store),
            // The branch above either returns an error or stores a handle, so this arm cannot be
            // reached. It is spelled out rather than `unwrap`ed because the workspace denies
            // `unwrap` in production code, and because a message costs nothing and a panic costs
            // the session.
            None => Err(ToolError::failed(
                "the index handle could not be opened and the reason was not recorded",
            )),
        }
    }

    /// The generation the reader was opened at, which goes stale when a watcher commits.
    ///
    /// The other half of the pair is [`Store::stored_generation`], and a caller that reports
    /// staleness compares them; see [`crate::tools::index`] for why that comparison is the only
    /// thing the field is for.
    #[must_use]
    pub fn opened_at_generation(&self) -> u64 {
        self.opened_at_generation
    }

    /// A short-lived write handle on the index, creating it if absent.
    ///
    /// Separate from [`Session::reader`] because `apply_update` needs `&mut`, and because a writer
    /// opened and closed per call leaves the reader's snapshot alone.
    pub fn writer(&mut self) -> Result<Store, ToolError> {
        let (repo, path) = self.identity()?;
        Store::open(path, repo).map_err(|error| {
            ToolError::failed(format!(
                "the index at {} could not be opened: {error}",
                path.display()
            ))
        })
    }

    /// Whether an index file exists for this repository.
    #[must_use]
    pub fn is_indexed(&self) -> bool {
        self.index_path.as_ref().is_some_and(|path| path.exists())
    }

    /// Refuse a write while a watch holds the writer.
    ///
    /// SQLite admits one writer at a time, so a concurrent `index` would either block for the busy
    /// timeout or fail. Refusing with the watch id is the honest version of both.
    pub fn refuse_if_watching(&self, what: &str) -> Result<(), ToolError> {
        match &self.watch {
            None => Ok(()),
            Some(running) => Err(ToolError::refused(
                format!(
                    "{what} cannot run while watch {} is running: SQLite admits one writer, and \
                     the watch holds it",
                    running.id
                ),
                "call `watch_stop` with that watch_id first, then run this again",
            )),
        }
    }

    /// The id of the running watch, if there is one.
    #[must_use]
    pub fn running_watch_id(&self) -> Option<u64> {
        self.watch.as_ref().map(|running| running.id)
    }

    /// What the watcher is doing, right now.
    #[must_use]
    pub fn watch_state(&self) -> Option<watch::WatchStatusView> {
        self.watch.as_ref().map(RunningWatch::status)
    }

    /// Start a watcher, refusing if one is already running.
    pub fn start_watch(
        &mut self,
        quiet_for: std::time::Duration,
        ready_timeout: std::time::Duration,
    ) -> Result<watch::StartedWatch, ToolError> {
        if let Some(running) = &self.watch {
            return Err(ToolError::refused(
                format!("watch {} is already running", running.id),
                "call `watch_stop` with that watch_id first; two watchers cannot both hold the \
                 single SQLite writer",
            ));
        }
        // Everything the session's own identity has to say is read into owned values *before* any
        // of it is mutated below, because `identity` borrows `self` and these lines write to it.
        // The repository identity itself is not needed here: the watcher thread resolves its own
        // store from the root, and a second identity in two places is a second thing to keep in
        // step.
        let (_repo, index_path) = self.identity()?;
        if !index_path.exists() {
            return Err(ToolError::not_indexed(&self.root));
        }
        let index_path = index_path.display().to_string();
        let root = self.root.clone();

        let id = self.next_watch_id;
        self.next_watch_id += 1;
        let running = watch::spawn(id, root.clone(), quiet_for, ready_timeout)?;
        self.log(&format!(
            "watch {id} watching {} (quiet for {} ms)",
            root.display(),
            quiet_for.as_millis()
        ));
        self.watch = Some(running);
        Ok(watch::StartedWatch {
            id,
            root: root.display().to_string(),
            quiet_for_ms: u64::try_from(quiet_for.as_millis()).unwrap_or(u64::MAX),
            ready_timeout_ms: u64::try_from(ready_timeout.as_millis()).unwrap_or(u64::MAX),
            index_path,
            watching: watch::watched_languages(),
        })
    }

    /// Stop the running watch, or a named one, and report what it did.
    pub fn stop_watch(&mut self, id: Option<u64>) -> Result<watch::StoppedWatch, ToolError> {
        // Taken rather than borrowed: `RunningWatch::stop` consumes the handle, and a type that
        // implements `Drop` cannot be moved out of a borrow.
        let running = self.watch.take().ok_or_else(|| {
            ToolError::refused(
                "no watch is running",
                "call `watch_start` first, or run `index` with `mode` set to `refresh` to update \
                 specific files once",
            )
        })?;
        if let Some(wanted) = id {
            if wanted != running.id {
                // Put it back before refusing: the caller asked about a watch that is not this
                // one, and this one is still running.
                let advice = format!("watch {} is running; stop that one", running.id);
                self.watch = Some(running);
                return Err(ToolError::refused(
                    format!("watch {wanted} is not the one running"),
                    advice,
                ));
            }
        }
        let watch_id = running.id;
        let stopped = running.stop();
        self.log(&format!(
            "watch {watch_id} stopped after {} applied refresh(es){}",
            stopped.applied,
            match stopped.stopped_cleanly {
                true => "",
                false => "; it did not stop cleanly",
            }
        ));
        Ok(stopped)
    }

    /// Record one diagnostic.
    pub fn log(&mut self, message: &str) {
        self.log.note(message);
    }

    /// Note that the client cancelled a request.
    pub fn cancel(&mut self, id: &serde_json::Value) {
        self.cancelled.insert(id_key(id));
        self.log(&format!("request {} cancelled before it ran", id_key(id)));
    }

    /// Whether a request was cancelled before it was dispatched.
    #[must_use]
    pub fn is_cancelled(&self, id: &serde_json::Value) -> bool {
        self.cancelled.contains(&id_key(id))
    }

    /// The text `initialize` sends back: how to use this server, in the terms a model needs.
    ///
    /// Built at call time rather than being a constant because one of the numbers in it is
    /// measured — the smallest budget a context pack can be compiled into, which is the cost of
    /// the report the compiler charges against every answer — and a constant would be a number
    /// that was true when it was written. When there is no index yet the number is not stated at
    /// all, rather than stated as zero.
    #[must_use]
    pub fn instructions(&mut self) -> String {
        let budget_sentence = match self.minimum_budget() {
            Some(floor) => format!(
                "A budget of at least {floor} tokens is the floor; below that the compiler \
                 refuses rather than returning context it cannot afford to explain."
            ),
            None => "Run `index` first. The compiler then reports the smallest budget it will \
                     accept, and refuses anything below it rather than exceeding it."
                .to_owned(),
        };
        let mut text = format!(
            "Peek indexes a local repository and answers structural questions about it. Nothing \
             leaves the machine: this server has no network socket of any kind.\n\n\
             Start with `index` if the repository has never been indexed — every other tool \
             answers from the index and says `not_indexed` rather than guessing. `index_status` \
             tells you whether it is, and where the index file is.\n\n\
             `context` is the tool to reach for when you need to read code: give it a target and \
             a token budget, and it returns a slice that fits inside the budget and names \
             everything it left out. {budget_sentence}\n\n\
             Edges carry how sure the engine is. An `ambiguous` edge lists the candidates it was \
             ambiguous between and is never presented as a fact; an `unresolved` edge says why it \
             could not be placed. If you need only proven edges, pass `follow_inferred: false`.\n\n\
             Every response carries `outcome`. `ok` and `reduced` are answers; `ambiguous_target` \
             carries candidates to choose from; `unknown_target` says which lookups missed; \
             `refused` says what would be accepted instead. None of them is an empty result."
        );
        if let Some(state) = self.watch_state() {
            if state.running {
                let _ = write!(
                    text,
                    "\n\nA watch ({}) is running. Call `watch_stop` with that watch_id when you \
                     are done, or this process stays alive.",
                    state.id
                );
            }
        }
        text
    }

    /// The smallest budget the context compiler will accept, or `None` when there is no index.
    ///
    /// It is the cost of the empty report — the sentence that says what was dropped — and the
    /// compiler charges it against every answer, so a budget below it cannot be honoured without
    /// returning something whose own explanation does not fit. Taken from the engine rather than
    /// recomputed here, so the number a caller is told and the number the compiler uses cannot be
    /// two constants that have drifted apart.
    pub fn minimum_budget(&mut self) -> Option<u64> {
        let store = self.reader().ok()?;
        Some(peek_core::query::Query::new(store).minimum_budget())
    }
}

/// A watcher running on its own thread.
pub struct RunningWatch {
    id: u64,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    state: Arc<watch::SharedState>,
    counters: Arc<watch::Counters>,
}

impl RunningWatch {
    /// What the watcher is doing, right now.
    fn status(&self) -> watch::WatchStatusView {
        watch::WatchStatusView {
            id: self.id,
            // A finished handle is how a watcher that died is visible: the thread cannot report
            // itself, and a status line that said "running" until the process exited would be a
            // claim nobody could check.
            running: self.join.as_ref().is_some_and(|handle| !handle.is_finished()),
            applied: self.counters.applied(),
            failed: self.counters.failed(),
            reports_superseded: self.counters.superseded(),
            last_refresh: self.state.last_refresh(),
            last_error: self.state.last_error(),
            root: self.state.root(),
            quiet_for_ms: self.state.quiet_for_ms(),
            watching: watch::watched_languages(),
        }
    }

    /// Ask the thread to stop, then wait for it.
    ///
    /// The join is what makes shutdown honest: the watcher thread flushes its open batch on the
    /// way out, so a change made in the moment before `watch_stop` is in the index rather than
    /// pending in a thread that was killed.
    fn stop(mut self) -> watch::StoppedWatch {
        self.stop.store(true, Ordering::SeqCst);
        let counters = CountersSnapshot {
            applied: self.counters.applied(),
            failed: self.counters.failed(),
            superseded: self.counters.superseded(),
        };
        let mut stopped_cleanly = true;
        let mut reason = None;
        // Spelled as a `match` rather than a let-chain, for the same reason as everywhere else in
        // this crate: see the note in `main.rs`.
        let ended_badly = match self.join.take() {
            None => false,
            Some(handle) => handle.join().is_err(),
        };
        if ended_badly {
            // A panic in a watcher is an index that claims to be current when it may not be, which
            // is the failure D-0009 is about. Reported rather than discarded.
            stopped_cleanly = false;
            reason = Some(
                "the watcher thread ended abnormally; its last refresh may not have been applied"
                    .to_owned(),
            );
        }
        watch::StoppedWatch {
            id: self.id,
            applied: counters.applied,
            failed: counters.failed,
            reports_superseded: counters.superseded,
            final_refresh: self.state.take_last_refresh(),
            stopped_cleanly,
            reason,
        }
    }
}

/// The three counters, read once so a stopping watch reports a consistent set.
///
/// Read together rather than one at a time because the thread is still running while they are
/// read, and three separately-read numbers could describe three different instants.
struct CountersSnapshot {
    applied: u64,
    failed: u64,
    superseded: u64,
}

impl Drop for RunningWatch {
    fn drop(&mut self) {
        // A session dropped with a watch running must not leave a thread behind holding the SQLite
        // writer in a process that is on its way out. Signalling is best-effort: the thread notices
        // within one poll interval, and a process that is exiting anyway will take it with it.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

/// The text form of a request id, used to match a cancellation against the request it names.
///
/// A number `7` and a string `"7"` are different ids in JSON-RPC, so this is not a correct
/// comparison on its own terms; it is a *key*, used only to remember that an id was cancelled. Both
/// spellings end up under the same key, which is what makes a cancellation effective whichever one
/// the client used, at the cost of a string id `"7"` cancelling a numeric id `7` as well. That is
/// the safe direction to be wrong in: a request the client no longer wants does not run.
fn id_key(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
