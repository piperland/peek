//! `index` and `index_status`: building the index, and measuring it.
//!
//! # `index` is the only writer, and it is the slowest thing here
//!
//! A full build of a large repository takes minutes and holds the SQLite write lock for the
//! duration. There is no way around that and no progress reporting in this protocol version, so
//! what the tool can do is report **honestly at the end**: the count in each of the five
//! resolution states, the files it could not read and why, and the write-ahead log size.
//!
//! # Why the five states are always all five
//!
//! The engine had a defect here and it was in the report, not the index: the five resolution
//! counts did not sum to the total they claimed to cover, so a reader could not tell work
//! remaining from work done. So this response carries a `resolution_states` object with all five
//! always present, the sum the engine computed, and a `states_partition` boolean saying whether
//! the five add up to `relations_written`. A mismatch is then visible rather than inferable.
//!
//! # Why the states are labelled with what they measure
//!
//! A build's five states and a status call's five states are different quantities under one name,
//! and a reader who assumes they are the same is wrong in the direction that matters. A build
//! reports the states the **extractor** left relations in, and the extraction pass is the *input*
//! to resolution rather than a result — so a build that resolved all nine of its pending relations
//! reports `pending: 9` beside `pending_remaining: false`, and read as a description of the index
//! it says the opposite of the truth.
//!
//! The engine is not at fault and this module does not paper over it. `IndexReport` documents its
//! own counters as the extractor's output and carries the resolver's decisions in a separate
//! `resolution` object. The honest per-run *post-pass* count is not derivable from either: for a
//! refresh the resolver also decides edges the run did not write, so adding its totals to the
//! extractor's would be arithmetic dressed as a measurement, and reading the store instead would
//! describe the whole index rather than the run. So the two are reported as the two measurements
//! they are — [`StatesMeasure`] travels with the counts — and the gap is closed by saying which is
//! which rather than by inventing a third number that neither of them supports.
//!
//! # Why `index_status` reports two generations
//!
//! [`Store::stats`] reports the generation its handle was *opened at*, because that is the value
//! the store caches and re-reading it would make one field mean two things. A long-lived reader
//! beside a running watcher therefore reports a number that is no longer true. So this tool
//! reports the handle's generation, the generation the index **currently records** — read from the
//! database, not from the cache — and whether they differ, rather than quietly serving a stale
//! figure under a field that says it is current.
//!
//! The second number is a second read of the same file, so it is not free; it is a single row in a
//! table the status call has already counted twice over. That is the price of a caller being able
//! to tell "this index is what I measured" from "this handle is behind what I measured", which is
//! the only thing this tool's generation fields exist to say.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use peek_core::containment::{normalise_lexically, relative_path, windows_drive_relative};
use peek_core::discover::DiscoveryOptions;
use peek_core::indexer::{self, IndexOutcome, IndexReport};
use peek_core::model::RepoPath;
use peek_core::resolve::ResolutionReport;
use peek_core::store::StoreStats;

use crate::outcome::{Outcome, ToolError, Verdict};
use crate::params::Args;
use crate::session::Session;
use crate::tools::ToolAnswer;

/// Build the whole index, or refresh named files.
pub fn index(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    let args = Args::new("index", arguments, crate::tool::hints_for("index"))?;
    let mode = args
        .optional_string("mode")?
        .unwrap_or_else(|| "full".to_owned());
    let paths = args.optional_string_array("paths")?;
    args.finish(&["mode", "paths"])?;

    if mode != "full" && mode != "refresh" {
        return Err(ToolError::argument(
            format!("`{mode}` is not a mode `index` has"),
            "`mode` is either `full`, which walks the whole repository, or `refresh`, which \
             re-extracts only `paths` and re-resolves the relations pointing into them",
        ));
    }

    // Both of these refusals are the fix for the predecessor's documented behaviour, where a
    // parameter that made no sense in the position it was given was accepted and ignored.
    if mode == "refresh" {
        let Some(paths) = paths.as_ref().filter(|paths| !paths.is_empty()) else {
            return Err(ToolError::argument(
                "`index` in `refresh` mode needs at least one path in `paths`",
                "give repository-relative paths such as `src/payments/service.rs`; use \
                 `mode: \"full\"` to walk the whole repository instead",
            ));
        };
        return refresh(session, paths);
    }
    if let Some(paths) = paths {
        if !paths.is_empty() {
            return Err(ToolError::argument(
                "`index` in `full` mode walks the whole repository, so `paths` would be ignored",
                "drop `paths` and call again, or set `mode` to \"refresh\" to re-index only the \
                 files you name",
            ));
        }
    }

    session.refuse_if_watching("a full build")?;
    let root = session.root().to_path_buf();
    let mut store = session.writer()?;
    let outcome =
        indexer::build_full(&mut store, &root, DiscoveryOptions::default()).map_err(|error| {
            ToolError::failed(format!(
                "the index for {} could not be built: {error}",
                root.display()
            ))
        })?;
    let view = IndexReportView::of(&outcome);
    session.log(&format!("index: {}", view.summary));
    Ok(ToolAnswer::new(
        view.render(),
        index_body(&view, &Verdict::ok()),
    ))
}

/// Re-extract and re-resolve the named files.
fn refresh(session: &mut Session, paths: &[String]) -> Result<ToolAnswer, ToolError> {
    let root = session.root().to_path_buf();
    // **The boundary, and it is before the writer is opened.** `Store::open` creates a missing
    // index, so a refusal that arrived after the store was open would leave behind an index for a
    // request it declined. The refusal is also before `refuse_if_watching`: a path outside the
    // repository is wrong whatever else is true of the session, and the watch would only add a
    // second refusal to answer once that one is fixed.
    let absolute = anchor(&root, paths)?;
    session.refuse_if_watching("a refresh")?;
    let mut store = session.writer()?;
    let outcome = indexer::refresh(&mut store, &root, &absolute, &DiscoveryOptions::default())
        .map_err(|error| {
            ToolError::failed(format!(
                "the refresh of {} path(s) under {} could not be applied: {error}",
                paths.len(),
                root.display()
            ))
        })?;
    let view = IndexReportView::of(&outcome);
    session.log(&format!("refresh: {}", view.summary));
    Ok(ToolAnswer::new(
        view.render(),
        index_body(&view, &Verdict::ok()),
    ))
}

/// Place every path the caller named inside `root`, or refuse the whole request.
///
/// # Why the refusal happens here and not in the engine
///
/// `Path::join` with an absolute argument **discards the base**: `root.join("/etc/passwd")` is
/// `/etc/passwd`, and whatever `root` was has no part in the result. So the path the caller supplied
/// is the path that gets read, and a client that can call this tool can name any file the host
/// process can read and get its contents back, indexed and summarised. The engine's `refresh` now
/// refuses to *index* such a path, but it refuses it the way it refuses an unreadable file: as a
/// **skip**, counted in `files_skipped` and named in the `skipped` list beside a build that
/// otherwise reports `outcome: ok`.
///
/// That is a successful answer to a build that did not read what it was asked to read. A skip is
/// "I looked and declined to act on this one"; a refusal is "this request is not something I will
/// do", with a reason and an `outcome` a caller can branch on. `peek rm` already has this wording
/// for the same condition, in `crates/peek-cli/src/paths.rs`, and the same sentences are used here
/// so the two surfaces cannot disagree about what the same request means.
///
/// # Why the whole request is refused rather than the offending entry
///
/// `paths` is a *request about a set*, not a set of independent requests. `index` with
/// `mode: "refresh"` means "these files are current now, re-extract them" — and the answer that
/// makes that claim true is one where every file named was acted on. Refusing the offending entry
/// alone would return a **plausible, successful, incomplete** answer: `outcome: ok`, `files_indexed`
/// counting the paths that worked, and the caller's real intent — that the index is current for
/// everything it named — silently unmet.
///
/// Three further reasons, in the order they decided it:
///
/// * **A partial answer is a wrong answer for this tool.** Every other field on the response is a
///   measurement, and a measurement with an unstated denominator is the defect this crate exists
///   to remove. A caller reading `files_indexed: 3` from a request for four paths has to
///   cross-reference the `skipped` list to notice, and the caller who was probing the boundary is
///   exactly the one who will not.
/// * **The caller can retry cheaply; it cannot discover cheaply.** Refusing names every offending
///   path, so one correction fixes the whole request in a single round trip. Partial execution
///   spends the run and leaves the caller to work out which entries are still missing.
/// * **The engine's skip is the right answer for the engine and the wrong one here.** A watcher
///   feeds `refresh` a batch of paths the operating system reported, and one unreadable file must
///   not stop the rest — so `SkipReason::OutsideRoot` earns its place. A caller's array is a
///   request, and a request is either honoured in full or refused in full.
///
/// The refusal names **every** offending entry rather than the first, because a caller that fixed
/// one path and re-sent the array would learn about the next one the same way, once per round trip,
/// and the engine has already proven it can decide all of them in one pass.
fn anchor(root: &Path, paths: &[String]) -> Result<Vec<PathBuf>, ToolError> {
    let mut anchored = Vec::with_capacity(paths.len());
    let mut refused: Vec<String> = Vec::new();
    for given in paths {
        match place(root, given) {
            Ok(path) => anchored.push(path),
            Err(reason) => refused.push(reason),
        }
    }
    if refused.is_empty() {
        return Ok(anchored);
    }
    let count = refused.len();
    Err(ToolError::refused(
        // **The reason is the refusal itself**, one sentence per offending entry in the CLI's
        // words, rather than a summary that defers the detail to `advice`. A caller reading only
        // `reason` — which is what most of them read, and what a client logs — has to learn *which*
        // path was wrong and *why*, and a summary that says "some of `paths` are not inside this
        // repository" answers neither. The summary of the outcome is the last clause, because
        // "nothing was indexed" is the part that decides what the caller does next.
        format!(
            "{} Nothing was indexed: {} of the {} path(s) in `paths` could not be placed, and \
             the request is refused in full rather than partly, because an answer covering only \
             the paths that could be honoured would tell the caller the index is current for \
             every path it named",
            refused.join(". "),
            count,
            paths.len(),
        ),
        format!(
            "`index` refreshes only files inside the repository at {}. Give `paths` paths \
             relative to it, `/`-separated, such as `src/payments/service.rs`",
            root.display()
        ),
    ))
}

/// One caller-supplied path, placed inside `root`.
///
/// **The join happens first and the judgement is made on its result**, which is the only order
/// that can work: anchoring an absolute path onto a root yields the absolute path, so a judgement
/// about the caller's spelling and a judgement about what will actually be read are two different
/// questions whenever the caller spelled one in full. Anchoring first makes the second one
/// askable; refusing before anchoring would be refusing a spelling and reading something else.
///
/// The two refusals are the CLI's sentences, kept as sentences rather than reworded, because they
/// are the wording a person has already learned to read and this is the same condition. The
/// drive-relative one is asked before the join: a path this process cannot resolve to a location has
/// no base to be joined onto and no location to be compared with, so it must not reach either.
fn place(root: &Path, given: &str) -> Result<PathBuf, String> {
    if given.is_empty() {
        return Err("the path is empty; name a file or directory inside the repository".to_owned());
    }
    if cfg!(windows) && windows_drive_relative(given) {
        return Err(format!(
            "{given} names a location relative to whatever directory the current drive happens to \
             be reading, which this process cannot see, so it cannot be shown to be inside the \
             repository at {}",
            root.display()
        ));
    }
    // **The join is what is judged, not the spelling**, and that is the whole difference from a
    // gate that ran before it: `root.join("/etc/passwd")` is `/etc/passwd`, so a spelling that is
    // outside is exactly the case a pre-join gate would have to inspect the join to find.
    let joined = root.join(given);
    // The engine's lexical rule, for the reason `containment` gives: the key has to be the key the
    // discovery walk would give the same file, so a path spelled in full that *is* inside the
    // repository is not refused for being spelled in full. It is asked in this order and against
    // this value because that is the sequence `indexer::refresh` applies to the same path, so a
    // path the boundary accepts and the engine skips is not reachable — the boundary can only turn
    // a skip into a refusal and never the other way round, which is what makes refusing the whole
    // array a change of what the answer *says* rather than a narrowing of what may be asked for.
    let Some(inside) = relative_path(root, &joined) else {
        return Err(outside_the_repository(
            given,
            &normalise_lexically(&joined),
            root,
        ));
    };
    // `RepoPath` is the model's own validator and the second half of the engine's predicate. A
    // spelling inside the root that names nothing — the root itself, `.`, `""` — has no name to
    // key an index row under, so it is refused here rather than becoming a skip there. Named by
    // the spelling rather than by the join, because on this branch the two are one file and the
    // caller is the one who wrote it.
    if RepoPath::from_path(&inside).is_none() {
        return Err(format!(
            "{given} is not a repository-relative path; a path that escapes the root, or is \
             empty, has no meaning here"
        ));
    }
    Ok(joined)
}

/// The refusal for a path that is not inside the repository, naming both places.
///
/// **The CLI's helper, with `read` where it says `touch`.** `outside_the_repository` in
/// `crates/peek-cli/src/paths.rs` refuses this condition with these words, and the actor differs
/// between the two surfaces — a command touches a working tree, this server reads a file to index
/// it — so the verb is the one word that had to change. Everything a reader acts on is the same,
/// including naming where the spelling points rather than the spelling itself, which is what tells
/// an escape apart from a spelling.
fn outside_the_repository(given: &str, spelled: &Path, root: &Path) -> String {
    let mut message = format!(
        "{} is not inside the repository at {}; this tool will not read anything outside the tree \
         it was pointed at",
        spelled.display(),
        root.display()
    );
    // The spelling and where it points differ whenever `..` was involved, and a reader told only
    // one of them cannot tell an escape from a spelling. Named once when they agree, twice when
    // they do not.
    if spelled.to_string_lossy() != given {
        message.push_str(&format!(". {given} names {}", spelled.display()));
    }
    message
}

/// Report what the index holds, measured.
pub fn status(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    let args = Args::new(
        "index_status",
        arguments,
        crate::tool::hints_for("index_status"),
    )?;
    args.finish(&[])?;

    let root = session.root().display().to_string();
    let Some(path) = session.index_path().map(Path::to_path_buf) else {
        return Err(ToolError::failed(format!(
            "no index location could be resolved for {root}"
        )));
    };
    let indexed = session.is_indexed();
    if !indexed {
        return Err(ToolError::not_indexed(session.root()));
    }

    // The reader is opened *before* the generation is read, because opening one is what sets the
    // generation it was opened at. Reading it first would report `0` on the first call of a
    // session and the right number on every call after — a figure that varies run to run for a
    // reason that has nothing to do with the index.
    let stats = session
        .reader()?
        .stats()
        .map_err(|error| ToolError::failed(format!("the index could not be measured: {error}")))?;
    let durability = session.reader()?.durability().as_str().to_owned();
    let opened_at = session.opened_at_generation();
    // Read from the database rather than from the handle's cache, which is the whole point: this
    // is the number the index carries *now*, and it moves when a watcher commits.
    let recorded = session.reader()?.stored_generation().map_err(|error| {
        ToolError::failed(format!(
            "the index's recorded generation could not be read: {error}"
        ))
    })?;
    // The store's own measurement, not a build report's. There is no run in this call, so an
    // `IndexReport` does not exist here, and the two are not the same number under a different
    // name: `IndexReport::relations_pending` is what the extractor *left* undecided before the
    // resolution pass ran, and `StoreStats::pending_relations` is what is still undecided in the
    // index at this moment. Quoting the first on a surface that claims to describe the index's
    // contents would be a build's input reported as the index's state. The check below is still a
    // real check, because all five counts come from one `GROUP BY resolution_state` over the whole
    // relation table: a state tag this build does not know leaves the sum short of
    // `relation_count` rather than dropping out of it.
    let states = ResolutionStates::of_stats(&stats);
    let accounted = states.total();
    let partition = accounted == stats.relation_count;
    let watch = session.watch_state();

    let body = serde_json::json!({
        "outcome": if partition { Outcome::Ok.as_str() } else { Outcome::Refused.as_str() },
        "reason": if partition {
            Value::Null
        } else {
            Value::String(format!(
                "the five resolution states add up to {accounted} but the index holds {} \
                 relations",
                stats.relation_count
            ))
        },
        "advice": if partition { Value::Null } else { Value::String("run `doctor`; a state that does not partition means the index was written by something that does not agree with this build".to_owned()) },
        "candidates": [],
        "repository": root,
        "index_path": path.display().to_string(),
        "schema_version": stats.schema_version,
        "generation": stats.generation,
        "opened_at_generation": opened_at,
        // What the index records at the moment of this call. The two above are this handle's, and
        // the two being different is the only thing `handle_is_stale` means.
        "recorded_generation": recorded,
        "handle_is_stale": recorded != opened_at,
        "durability": durability,
        "states_partition": partition,
        "resolution_states": states,
        "stats": stats,
        "watch": watch,
    });
    let text = format!(
        "generation {} at {}\n{}",
        stats.generation,
        path.display(),
        states.render(stats.relation_count, partition)
    );
    Ok(ToolAnswer::new(text, body))
}

fn index_body(view: &IndexReportView, verdict: &Verdict) -> Value {
    let body = IndexBody {
        verdict: verdict.clone(),
        report: view.clone(),
    };
    serde_json::to_value(&body).unwrap_or_else(|error| {
        serde_json::json!({
            "outcome": "failed",
            "reason": format!("the index report could not be encoded: {error}"),
            "advice": Value::Null,
            "candidates": [],
        })
    })
}

/// The body of an `index` answer.
#[derive(Debug, Serialize)]
struct IndexBody {
    #[serde(flatten)]
    verdict: Verdict,
    report: IndexReportView,
}

/// The five resolution states, and which measurement they are.
///
/// All five are always present. A reader who has to notice a missing key to know a state is absent
/// is a reader who will not notice.
///
/// # Why this is one type used for two measurements
///
/// `index` and `index_status` both report five numbers under the same name, and the two are **not**
/// the same quantity. `index` reports the states of the relations *one run wrote*, counted as the
/// extractor left them — the input to the resolution pass, which is why a fully successful build
/// can report a non-zero `pending` here. `index_status` reports the states the *store* measures
/// across the whole index at the moment of the call, which is the only one of the two that
/// answers "how good is this index".
///
/// A payload carrying the first under a name that reads like the second is the worst of both: a
/// caller is told the index is worse than it is, and every accuracy number downstream of it is
/// wrong in the pessimistic direction. So the measurement travels with the counts — see
/// [`StatesMeasure`] — rather than being left to the field name and a reader's inference. The
/// engine is not at fault here: `IndexReport` documents its own counters as the extractor's
/// output and carries the pass's decisions in a separate field, and this module passes both
/// through rather than reconciling them into one number it cannot measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolutionStates {
    /// Which measurement the five counts below are.
    pub measured: StatesMeasure,
    /// Proven: exactly one target, with evidence that supports it.
    pub resolved: u64,
    /// Extracted but not decided **yet**.
    ///
    /// Under [`StatesMeasure::RunAsExtracted`] this counts relations the pass was given and then
    /// decided, so it is not outstanding work; `IndexReport::resolution` says how it went. Under
    /// [`StatesMeasure::IndexAsStored`] it is exactly the work remaining in the index.
    pub pending: u64,
    /// More than one candidate fits and none was proven correct.
    pub ambiguous: u64,
    /// No target could be established, each with a stated reason.
    pub unresolved: u64,
    /// Derived from another fact, with the derivation recorded in the edge.
    pub inferred: u64,
}

/// Which measurement a [`ResolutionStates`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatesMeasure {
    /// The relations one indexing run wrote, in the states the extractor left them in.
    ///
    /// The extraction pass is the *input* to resolution, never a result: the extractor does not
    /// decide anything, so every count here was true before the resolver ran. The result is the
    /// run's `resolution` object.
    RunAsExtracted,
    /// The whole index, as the store's own `GROUP BY resolution_state` measures it now.
    IndexAsStored,
}

impl ResolutionStates {
    /// The counts an index report produced.
    #[must_use]
    pub fn of(report: &IndexReport) -> Self {
        Self {
            measured: StatesMeasure::RunAsExtracted,
            resolved: report.relations_resolved,
            pending: report.relations_pending,
            ambiguous: report.relations_ambiguous,
            unresolved: report.relations_unresolved,
            inferred: report.relations_inferred,
        }
    }

    /// The counts a store's own measurement produced.
    #[must_use]
    pub fn of_stats(stats: &StoreStats) -> Self {
        Self {
            measured: StatesMeasure::IndexAsStored,
            resolved: stats.resolved_relations,
            pending: stats.pending_relations,
            ambiguous: stats.ambiguous_relations,
            unresolved: stats.unresolved_relations,
            inferred: stats.inferred_relations,
        }
    }

    /// The sum of the five.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.resolved + self.pending + self.ambiguous + self.unresolved + self.inferred
    }

    /// One readable line, and the check that the five add up to `total`.
    fn render(self, total: u64, partition: bool) -> String {
        format!(
            "{} relations: {} resolved, {} inferred, {} ambiguous, {} unresolved, {} pending\n\
             partition: {} — {} of {} accounted for",
            total,
            self.resolved,
            self.inferred,
            self.ambiguous,
            self.unresolved,
            self.pending,
            if partition { "yes" } else { "NO" },
            self.total(),
            total,
        )
    }
}

/// One indexing run, as the caller sees it.
#[derive(Debug, Clone, Serialize)]
pub struct IndexReportView {
    /// The store generation after the commit. Never fabricated.
    pub generation: u64,
    pub files_indexed: u64,
    pub files_skipped: u64,
    pub files_unsupported: u64,
    pub files_degraded: u64,
    pub files_removed: u64,
    pub entities_written: u64,
    pub entities_removed: u64,
    pub relations_written: u64,
    /// The five states, all of them.
    pub resolution_states: ResolutionStates,
    /// What the five sum to, and whether it equals `relations_written`.
    pub relations_accounted_for: u64,
    pub states_partition: bool,
    pub tests_found: u64,
    /// Size of the write-ahead log after the run. A full build checkpoints, so a large number here
    /// means a checkpoint was refused by a concurrent reader.
    pub wal_bytes: u64,
    /// Wall-clock time, in milliseconds. The one number in a response that is not reproducible,
    /// which is why the determinism test in `tests/` does not cover `index`.
    pub elapsed_ms: u64,
    /// What the second pass decided.
    pub resolution: Option<ResolutionView>,
    /// Every file that was considered and not indexed, with the reason.
    pub skipped: Vec<SkippedView>,
    /// The engine's own one-line summary, verbatim.
    pub summary: String,
}

impl IndexReportView {
    /// The view of an outcome.
    #[must_use]
    pub fn of(outcome: &IndexOutcome) -> Self {
        let report = outcome.report();
        let states = ResolutionStates::of(report);
        let accounted = states.total();
        Self {
            generation: report.generation,
            files_indexed: report.files_indexed,
            files_skipped: report.files_skipped,
            files_unsupported: report.files_unsupported,
            files_degraded: report.files_degraded,
            files_removed: report.files_removed,
            entities_written: report.entities_written,
            entities_removed: report.entities_removed,
            relations_written: report.relations_written,
            resolution_states: states,
            relations_accounted_for: accounted,
            states_partition: accounted == report.relations_written,
            tests_found: report.tests_found,
            wal_bytes: report.wal_bytes,
            elapsed_ms: u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
            resolution: report.resolution.as_ref().map(ResolutionView::of),
            skipped: outcome
                .skipped
                .iter()
                .map(|file| SkippedView {
                    path: file.path.clone(),
                    reason: file.reason.to_string(),
                })
                .collect(),
            summary: report.summary(),
        }
    }

    /// A readable report, worst thing first.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = self.summary.clone();
        // Labelled, not bare. The same five words beside `pending: 9` and `pending_remaining:
        // false` read as a contradiction unless the reader is told that the first line is the
        // extractor's output and the second is the pass's decision about it.
        text.push_str(&format!(
            "\nas extracted: {} resolved, {} inferred, {} ambiguous, {} unresolved, {} pending",
            self.resolution_states.resolved,
            self.resolution_states.inferred,
            self.resolution_states.ambiguous,
            self.resolution_states.unresolved,
            self.resolution_states.pending
        ));
        if let Some(resolution) = &self.resolution {
            text.push_str(&format!(
                "\nafter the resolution pass: {} examined, {} resolved, {} inferred, {} ambiguous, \
                 {} unresolved, pending remaining: {}",
                resolution.examined,
                resolution.resolved,
                resolution.inferred,
                resolution.ambiguous,
                resolution.unresolved,
                resolution.pending_remaining
            ));
        }
        text.push_str(&format!(
            "\npartition: {} — {} of {} relations accounted for",
            if self.states_partition { "yes" } else { "NO" },
            self.relations_accounted_for,
            self.relations_written
        ));
        for file in &self.skipped {
            text.push_str(&format!("\nskipped {}: {}", file.path, file.reason));
        }
        text
    }
}

/// What the resolution pass decided.
#[derive(Debug, Clone, Serialize)]
pub struct ResolutionView {
    pub examined: u64,
    pub resolved: u64,
    pub inferred: u64,
    pub ambiguous: u64,
    pub unresolved: u64,
    /// Unresolved by reason, so "why" is a number rather than a guess.
    pub unresolved_by_reason: std::collections::BTreeMap<String, u64>,
    pub reconsidered: u64,
    pub displaced: u64,
    pub relations_written: u64,
    /// Candidate lookups abandoned at a limit. Non-zero means this build's answer may be less
    /// complete than the index could have supported, and it is the only way to know.
    pub truncated: u64,
    /// Whether any `Pending` relation is still in the index. A true here is unfinished work, not a
    /// defect, and the two look the same in a count.
    pub pending_remaining: bool,
    pub committed: bool,
    pub generation: u64,
    /// The engine's own one-line summary.
    pub summary: String,
}

impl ResolutionView {
    fn of(report: &ResolutionReport) -> Self {
        Self {
            examined: report.examined,
            resolved: report.resolved,
            inferred: report.inferred,
            ambiguous: report.ambiguous,
            unresolved: report.unresolved,
            unresolved_by_reason: report.unresolved_by_reason.clone(),
            reconsidered: report.reconsidered,
            displaced: report.displaced,
            relations_written: report.relations_written,
            truncated: report.truncated,
            pending_remaining: report.pending_remaining,
            committed: report.committed,
            generation: report.generation,
            summary: report.summary(),
        }
    }
}

/// One file that was considered and not indexed.
#[derive(Debug, Clone, Serialize)]
pub struct SkippedView {
    pub path: String,
    /// The engine's own rendering of the reason, verbatim.
    pub reason: String,
}
