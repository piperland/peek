//! The answer types: one value per command, rendered two ways.
//!
//! # Why the JSON mode is derived rather than written
//!
//! Contract C3: the CLI and the MCP server are adapters over one typed core, and no logic is
//! duplicated between them. The trap in a CLI is that the human text and the JSON drift — someone
//! adds a field to the struct, forgets the printer, and two surfaces describe the same answer
//! differently.
//!
//! So a command produces **one value**, [`Answer`], and the two output modes are two functions
//! from it. The JSON mode is `serde_json` over the value itself, which means it is complete by
//! construction: there is no field a human line can carry and the JSON cannot. The MCP surface
//! will serialise the same value, and the only thing it will have to add is a tool schema.
//!
//! # What every answer carries
//!
//! * **`notes`** — statements the answer owes its reader. A bound that was hit, a candidate that
//!   was never examined, an answer that turned out to be complete.
//! * **`did_not`** — what this command could not do. Never empty on purpose: a command that did
//!   everything still has a version it did not support, a check it did not run, and a file class
//!   it did not read. Printing that list is the difference between a tool and a rumour.
//!
//! The human renderer prints both. The JSON mode carries both, always, under those names.
//!
//! # A command that did not answer carries the refusal as its answer
//!
//! [`Answer::Declined`] is the variant for a usage error, a refusal and an engine failure, and it
//! exists because none of the three has an answer and every one of them has a *reason*.
//!
//! The defect it fixes was this: those three cases were reported with the usage text in the
//! `answer` field, unconditionally, so the status and the exit code were right and a person at a
//! terminal saw the reason — while `--json`, a script and an agent, all of which read the answer,
//! got the command list. The reason existed in the output and nowhere a machine would look.
//!
//! So a refusal is information, and it is the answer when there is no answer. The alternative —
//! leaving the field to mean "the question was not answered, and the envelope's `refusal` says
//! why" — is what the type was doing, and it is the thing this crate has argued against from its
//! first audit: a value whose meaning depends on a field somewhere else.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use peek_core::doctor::{Check, Finding, Severity};
use peek_core::indexer::{IndexReport, SkippedFile};
use peek_core::query::{ContextPack, Explanation, Walk};
use peek_core::resolve::ResolutionReport;
use peek_core::store::StoreStats;

use crate::exit::{Failure, Refusal, Status};

/// What a command produced.
///
/// Internally tagged by command name, so the JSON mode's `answer` object is self-describing: a
/// consumer can tell what it received without having agreed a schema out of band.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Answer {
    /// The usage text.
    Help(HelpAnswer),
    /// The version.
    Version(VersionAnswer),
    /// The result of an indexing run.
    Index(IndexAnswer),
    /// What a watch session applied.
    Watch(WatchAnswer),
    /// Why a declaration exists.
    Explain(ExplainAnswer),
    /// A structural traversal. One kind for all three names; which name was used is on the
    /// payload. See [`Answer::command`].
    Walk(WalkAnswer),
    /// A token-budgeted context pack.
    Context(ContextAnswer),
    /// A diagnosis.
    Doctor(DoctorAnswer),
    /// The index's own numbers.
    Status(StatusAnswer),
    /// What a removal took out.
    Remove(RemoveAnswer),
    /// A command that did not answer, and why.
    ///
    /// Its own variant rather than a stand-in, because a usage error, a refusal and an engine
    /// failure have no answer and the only thing they do have is the reason. See the module
    /// documentation.
    Declined(DeclinedAnswer),
}

impl Answer {
    /// The kind of answer this is, matching the tag in the JSON mode.
    ///
    /// **`callers`, `callees` and `dependents` are one answer kind.** They are one walk with
    /// different parameters (D-0007), and which of the three names the caller used is on
    /// [`WalkAnswer::command`], inside the answer. This is the answer's *shape*, not the name it
    /// was invoked by; [`Output::command`](crate::Output::command) is the latter.
    #[must_use]
    pub fn command(&self) -> &'static str {
        match self {
            Answer::Help(_) => "help",
            Answer::Version(_) => "version",
            Answer::Index(_) => "index",
            Answer::Watch(_) => "watch",
            Answer::Explain(_) => "explain",
            Answer::Walk(_) => "walk",
            Answer::Context(_) => "context",
            Answer::Doctor(_) => "doctor",
            Answer::Status(_) => "status",
            Answer::Remove(_) => "rm",
            Answer::Declined(_) => "declined",
        }
    }

    /// The human rendering: the answer body, its notes, and what it could not do.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Answer::Help(answer) => answer.usage.clone(),
            Answer::Version(answer) => answer.render(),
            Answer::Index(answer) => answer.render(),
            Answer::Watch(answer) => answer.render(),
            Answer::Explain(answer) => answer.render(),
            Answer::Walk(answer) => answer.render(),
            Answer::Context(answer) => answer.render(),
            Answer::Doctor(answer) => answer.render(),
            Answer::Status(answer) => answer.render(),
            Answer::Remove(answer) => answer.render(),
            Answer::Declined(answer) => answer.render(),
        }
    }

    /// Everything this command could not do.
    ///
    /// Empty only for `help` and `version`, which read nothing and so have nothing to have failed
    /// at. Every other command's list is non-empty by construction, and a test asserts it — which
    /// includes a command that declined, because a failure that carried an empty list would read as
    /// a complete answer that happened to be short.
    #[must_use]
    pub fn did_not(&self) -> &[String] {
        match self {
            Answer::Help(_) | Answer::Version(_) => &[],
            Answer::Index(answer) => &answer.did_not,
            Answer::Watch(answer) => &answer.did_not,
            Answer::Explain(answer) => &answer.did_not,
            Answer::Walk(answer) => &answer.did_not,
            Answer::Context(answer) => &answer.did_not,
            Answer::Doctor(answer) => &answer.did_not,
            Answer::Status(answer) => &answer.did_not,
            Answer::Remove(answer) => &answer.did_not,
            Answer::Declined(answer) => &answer.did_not,
        }
    }

    /// Everything the answer owes its reader.
    ///
    /// Empty for a declined answer, whose only obligation is the reason and which carries it on
    /// [`DeclinedAnswer::refusal`]. A statement that repeated it here would be printed twice.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        match self {
            Answer::Index(answer) => &answer.notes,
            Answer::Watch(answer) => &answer.notes,
            Answer::Context(answer) => &answer.notes,
            Answer::Remove(answer) => &answer.notes,
            Answer::Explain(_)
            | Answer::Walk(_)
            | Answer::Doctor(_)
            | Answer::Status(_)
            | Answer::Declined(_) => &[],
            Answer::Help(_) | Answer::Version(_) => &[],
        }
    }

    /// Whether this answer's own rendering already states the refusal.
    ///
    /// The envelope prints a refusal after the answer, because a `doctor` that found a failing
    /// check and a `context` pack that could not fit its target are answers *and* have something to
    /// say about their limits, and both are wanted. A declined answer is the refusal, so printing
    /// it again would state the same paragraph twice — and the reader would have to work out which
    /// of the two was the answer.
    #[must_use]
    pub fn states_its_refusal(&self) -> bool {
        matches!(self, Answer::Declined(_))
    }
}

/// Render a list of statements under a heading, or nothing at all when it is empty.
fn section(heading: &str, lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut text = String::from(heading);
    for line in lines {
        text.push_str(&format!("\n  {line}"));
    }
    text
}

/// The usage text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelpAnswer {
    /// Every line, generated from the flag and command tables.
    pub usage: String,
}

impl HelpAnswer {
    /// The human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        self.usage.clone()
    }
}

/// The version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionAnswer {
    /// This binary's version, which is the workspace version.
    pub version: String,
    /// The engine version, which is the same string today and is reported separately because the
    /// two *could* diverge: a binary and the engine it links are separate units of release.
    pub engine_version: String,
}

impl VersionAnswer {
    /// The human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "peek {}\nengine {}\nschema v{}\n",
            self.version,
            self.engine_version,
            peek_core::store::SCHEMA_VERSION
        )
    }
}

/// A file that was considered and not indexed, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFileAnswer {
    /// Repository-relative, or the literal path as given when none could be computed.
    pub path: String,
    /// The reason, in the indexer's own vocabulary.
    pub reason: String,
}

impl From<&SkippedFile> for SkippedFileAnswer {
    fn from(skipped: &SkippedFile) -> Self {
        Self {
            path: skipped.path.clone(),
            reason: skipped.reason.to_string(),
        }
    }
}

/// What the resolver decided in the second pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionAnswer {
    /// Which pass this was. It is named rather than implied, because a caller comparing two runs
    /// needs to know that a different pass produced each number.
    pub pass: String,
    /// Relations the pass looked at.
    pub examined: u64,
    /// Proven to have exactly one target.
    pub resolved: u64,
    /// Bound by inference rather than observation, with the claim written into the relation.
    pub inferred: u64,
    /// More than one equally supported candidate; every one was recorded.
    pub ambiguous: u64,
    /// No target could be established.
    pub unresolved: u64,
    /// Unresolved relations by reason, so "why" is a number.
    pub unresolved_by_reason: BTreeMap<String, u64>,
    /// Relations that were already decided and were decided again.
    pub reconsidered: u64,
    /// Edges the caller read before its write, whose targets that write removed.
    pub displaced: u64,
    /// Rows the pass rewrote.
    pub relations_written: u64,
    /// Candidate lookups abandoned at a configured limit.
    pub truncated: u64,
    /// Whether any relation is still undecided after the pass.
    pub pending_remaining: bool,
    /// Whether the pass committed. A pass that decided nothing does not.
    pub committed: bool,
    /// The generation after the pass.
    pub generation: u64,
}

impl From<&ResolutionReport> for ResolutionAnswer {
    fn from(report: &ResolutionReport) -> Self {
        Self {
            pass: ResolutionReport::PASS.to_owned(),
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
        }
    }
}

/// The report the binary printed, and the code it exited with.
///
/// Both halves are needed together. An agent branching on the code needs the text to say *which*
/// refusal it got, and a human reading the text needs the code to know whether a script that
/// printed it succeeded.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The human or JSON rendering, complete and with a trailing newline.
    pub text: String,
    /// The code the process exits with.
    pub exit_code: u8,
    /// Whether the rendering is JSON, so the binary knows not to print usage after it.
    pub json: bool,
}

impl Report {
    /// A report, with the code derived from the status so the two cannot disagree.
    #[must_use]
    pub fn new(text: String, status: Status, json: bool) -> Self {
        Self {
            text,
            exit_code: status.exit_code(),
            json,
        }
    }
}

/// A failure, rendered the same way an answer is, with the same code beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Failed {
    /// The rendering, with a trailing newline.
    pub text: String,
    /// The code the process exits with.
    pub exit_code: u8,
    /// Whether the rendering is JSON.
    pub json: bool,
    /// The usage text for the command that failed, for the human mode. Empty in the JSON mode,
    /// which has no prose.
    pub usage: String,
}

/// What an indexing run did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexAnswer {
    /// `build` for a cold index, `refresh` for an incremental one, `rebuild` for `--full`.
    pub mode: String,
    /// The store generation after the commit. Never fabricated.
    pub generation: u64,
    /// Files read and extracted.
    pub files_indexed: u64,
    /// Files whose rows were removed.
    pub files_removed: u64,
    /// Files that could not be read or decoded.
    pub files_skipped: u64,
    /// Files whose language has no extraction rules. **Not** the same as "no symbols".
    pub files_unsupported: u64,
    /// Files whose parse was degraded, so their extraction is best effort.
    pub files_degraded: u64,
    /// How many of `files_removed` were files the discovery walk no longer yields, which is the
    /// only way a deleted file can be noticed and the thing a plain refresh would otherwise miss.
    pub stale_paths_removed: u64,
    /// Top-level entries cleared before a `--full` rebuild.
    pub cleared_paths: Vec<String>,
    pub entities_written: u64,
    pub entities_removed: u64,
    pub relations_written: u64,
    /// Entities that are test cases.
    pub tests_found: u64,
    /// Every file that was considered and not indexed, with the reason.
    pub skipped: Vec<SkippedFileAnswer>,
    /// The same refusals, grouped, so a reader who only wants the shape gets it in one line.
    pub skipped_by_reason: BTreeMap<String, u64>,
    /// What the second pass decided, or `None` when it did not run.
    pub resolution: Option<ResolutionAnswer>,
    /// Whether the five resolution states the report counts add up to the relations it wrote.
    ///
    /// Asserted rather than displayed. A report whose states do not partition is a report that
    /// cannot be checked, and a reader cannot tell work remaining from work done without it.
    pub resolution_states_partition: bool,
    /// Wall-clock milliseconds.
    pub elapsed_ms: u64,
    /// Statements the run owes its reader.
    pub notes: Vec<String>,
    /// What this run could not do.
    pub did_not: Vec<String>,
}

impl IndexAnswer {
    /// The human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        // The repository and the index file are printed once by the envelope; the body carries the
        // run's own numbers and nothing else.
        let mut text = format!(
            "peek index ({})\n\
             generation: {}\n\
             files:      {} indexed, {} removed, {} skipped, {} unsupported, {} degraded\n\
             entities:   {} written, {} removed\n\
             relations:  {} written, {} test(s)\n",
            self.mode,
            self.generation,
            self.files_indexed,
            self.files_removed,
            self.files_skipped,
            self.files_unsupported,
            self.files_degraded,
            self.entities_written,
            self.entities_removed,
            self.relations_written,
            self.tests_found
        );
        if self.stale_paths_removed > 0 {
            text.push_str(&format!(
                "deleted:    {} indexed file(s) are no longer on disk and were removed\n",
                self.stale_paths_removed
            ));
        }
        if !self.cleared_paths.is_empty() {
            text.push_str(&format!(
                "cleared:    {} top-level entr(ies) before the rebuild: {}\n",
                self.cleared_paths.len(),
                self.cleared_paths.join(", ")
            ));
        }
        if let Some(resolution) = &self.resolution {
            text.push_str(&format!(
                "resolution: examined {}, resolved {}, inferred {}, ambiguous {}, unresolved {}\n",
                resolution.examined,
                resolution.resolved,
                resolution.inferred,
                resolution.ambiguous,
                resolution.unresolved
            ));
            if !resolution.unresolved_by_reason.is_empty() {
                let reasons: Vec<String> = resolution
                    .unresolved_by_reason
                    .iter()
                    .map(|(reason, count)| format!("{reason} {count}"))
                    .collect();
                text.push_str(&format!("unresolved by reason: {}\n", reasons.join(", ")));
            }
            if resolution.truncated > 0 {
                text.push_str(&format!(
                    "truncated:  {} candidate lookup(s) hit a limit\n",
                    resolution.truncated
                ));
            }
        } else {
            text.push_str("resolution: did not run\n");
        }
        if !self.resolution_states_partition {
            text.push_str(
                "resolution states do not add up to the relations written; the indexer's own report \
                 is inconsistent and this run's counts cannot be checked\n",
            );
        }
        if !self.skipped_by_reason.is_empty() {
            let reasons: Vec<String> = self
                .skipped_by_reason
                .iter()
                .map(|(reason, count)| format!("{count} {reason}"))
                .collect();
            text.push_str(&format!("skipped:    {}\n", reasons.join(", ")));
        }
        if !self.skipped.is_empty() {
            text.push_str("not indexed:\n");
            for entry in &self.skipped {
                text.push_str(&format!("  {}: {}\n", entry.path, entry.reason));
            }
        }
        text.push_str(&format!("elapsed:    {} ms\n", self.elapsed_ms));
        text.push_str(&section("note:", &self.notes));
        text.push_str(&section("\ncould not:", &self.did_not));
        text
    }
}

/// A path the watcher declined, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionAnswer {
    /// As the watcher saw it, absolute.
    pub path: String,
    /// Why it was declined.
    pub reason: String,
}

/// What a watch session applied, over its whole life.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchAnswer {
    /// The quiet period in force.
    pub quiet_for_ms: u64,
    /// The age at which a batch was closed regardless of the quiet period.
    pub max_batch_ms: u64,
    /// Batches applied.
    pub batches_applied: u64,
    /// Raw filesystem events coalesced into those batches.
    pub events_seen: u64,
    /// Distinct paths re-indexed.
    pub paths_reindexed: u64,
    /// Declined paths, grouped by the reason the plan gave.
    ///
    /// **Complete**: every declined path over the whole session is counted here.
    pub ignored_by_reason: BTreeMap<String, u64>,
    /// A bounded sample of the declined paths, so the count is not the only evidence.
    ///
    /// Bounded because a watcher over a real repository declines thousands of files over a session
    /// and itemising every one would bury the batch it just applied. The count beside the sample is
    /// the complete number, so nothing is lost — only the tail of a long list is not itemised.
    pub ignored_sample: Vec<DecisionAnswer>,
    /// Paths that arrived from outside the repository, which means the watcher is misconfigured.
    pub outside_root: u64,
    /// Files the indexer refused, grouped by reason, across every batch. Complete.
    pub skipped_by_reason: BTreeMap<String, u64>,
    /// A bounded sample of those files, for the same reason as [`Self::ignored_sample`].
    pub skipped_sample: Vec<SkippedFileAnswer>,
    /// The generation before the first batch, if there was one.
    pub first_generation: Option<u64>,
    /// The generation after the last batch.
    pub last_generation: Option<u64>,
    /// Every batch that failed to apply. Non-empty means the index is behind the working tree.
    pub failures: Vec<String>,
    /// Statements the session owes its reader.
    pub notes: Vec<String>,
    /// What this session could not do.
    pub did_not: Vec<String>,
}

impl WatchAnswer {
    /// The human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "peek watch\n\
             quiet for:  {} ms, at most {} ms per batch\n\
             batches:    {} applied from {} event(s)\n\
             re-indexed: {} distinct path(s)\n",
            self.quiet_for_ms,
            self.max_batch_ms,
            self.batches_applied,
            self.events_seen,
            self.paths_reindexed
        );
        if let (Some(first), Some(last)) = (self.first_generation, self.last_generation) {
            text.push_str(&format!("generation: {first} then {last}\n"));
        }
        if !self.ignored_by_reason.is_empty() {
            let reasons: Vec<String> = self
                .ignored_by_reason
                .iter()
                .map(|(reason, count)| format!("{count} {reason}"))
                .collect();
            text.push_str(&format!("declined:    {}\n", reasons.join(", ")));
        }
        for decision in &self.ignored_sample {
            text.push_str(&format!(
                "  declined {}: {}\n",
                decision.path, decision.reason
            ));
        }
        if self.outside_root > 0 {
            text.push_str(&format!(
                "outside the repository: {} path(s); the watcher is watching something it was not \
                 asked to watch\n",
                self.outside_root
            ));
        }
        if !self.skipped_by_reason.is_empty() {
            let reasons: Vec<String> = self
                .skipped_by_reason
                .iter()
                .map(|(reason, count)| format!("{count} {reason}"))
                .collect();
            text.push_str(&format!("not indexed: {}\n", reasons.join(", ")));
        }
        for entry in &self.skipped_sample {
            text.push_str(&format!("  {}: {}\n", entry.path, entry.reason));
        }
        if self.failures.is_empty() {
            text.push_str("failures:    none\n");
        } else {
            text.push_str(&format!("failures:    {}\n", self.failures.len()));
            for failure in &self.failures {
                text.push_str(&format!("  {failure}\n"));
            }
        }
        text.push_str(&section("note:", &self.notes));
        text.push_str(&section("\ncould not:", &self.did_not));
        text
    }
}

/// Why a declaration exists and what it points at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplainAnswer {
    /// The string as typed.
    pub target: String,
    /// Which of the engine's three lookups matched: `path`, `qualified_name` or `name`.
    pub matched: String,
    /// The engine's answer, whole, so the JSON mode is complete without re-deriving anything.
    pub explanation: Explanation,
    /// How many of the edges listed have no proven target. D-0004: this is as much a part of the
    /// answer as the certain ones, so it is a number rather than something to count by reading.
    pub uncertain_edges: u64,
    /// What this command could not do.
    pub did_not: Vec<String>,
}

impl ExplainAnswer {
    /// The human rendering: the engine's own, with the summary this command adds.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "peek explain: {target:?} matched by {matched}\n{explanation}",
            target = self.target,
            matched = self.matched,
            explanation = self.explanation.render()
        );
        text.push_str(&format!(
            "\n{} edge(s) listed, {uncertain} of them with no proven target",
            self.explanation.edges.len(),
            uncertain = self.uncertain_edges
        ));
        text.push_str(&section("\ncould not:", &self.did_not));
        text
    }
}

/// A structural traversal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkAnswer {
    /// Which of the three names the caller used. They are one walk with different parameters, and
    /// the answer says which one so a reader does not have to infer it from the direction.
    ///
    /// Serialised as `command_name` because the enclosing variant's tag is already `command` and
    /// carries the answer's *kind*; see [`Answer::command`].
    #[serde(rename = "command_name")]
    pub command: String,
    /// The string as typed.
    pub target: String,
    /// Which of the engine's three lookups matched.
    pub matched: String,
    /// The engine's answer, whole.
    pub walk: Walk,
    /// How many of the steps arrived through an inferred edge. Always equal to the number of steps
    /// whose `via.resolution` is inferred, so a caller can check the count against the list.
    pub inferred_steps: u64,
    /// What this command could not do.
    pub did_not: Vec<String>,
}

impl WalkAnswer {
    /// The human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "peek {}: {target:?} matched by {matched}\n{headline}\n",
            self.command,
            target = self.target,
            matched = self.matched,
            headline = self.walk.headline()
        );
        for step in &self.walk.steps {
            text.push_str(&format!(
                "  {distance} {place} — {path}:{line} via {kind} [{state}]\n",
                distance = step.distance,
                place = step.id.display(),
                path = step.via.source.path(),
                line = step.via.span.start_line,
                kind = step.via.kind,
                state = step.state()
            ));
        }
        if self.walk.steps.is_empty() {
            text.push_str("  nothing was reached\n");
        }
        if self.walk.seeds.is_empty() {
            text.push_str(
                "  the target is a declaration, not a container, so it was not expanded\n",
            );
        } else {
            text.push_str(&format!(
                "  the target is a container and expanded to {} member(s) first\n",
                self.walk.seeds.len()
            ));
        }
        text.push_str(&format!(
            "  {inferred} of {total} step(s) arrived by inference\n",
            inferred = self.inferred_steps,
            total = self.walk.steps.len()
        ));
        text.push_str(&section("\ncould not:", &self.did_not));
        text
    }
}

/// A token-budgeted context pack, with the budget arithmetic the engine does not carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextAnswer {
    /// The pack, whole. The budget report, the report text, the units, the omissions and the
    /// engine's notes are all on it, so the JSON mode is complete by carrying it.
    pub pack: ContextPack,
    /// The smallest budget `peek` will accept: the cost of the empty report. Obtainable here
    /// before asking, so a caller can reject a hopeless budget without paying for a compilation.
    pub minimum_budget: u64,
    /// The smallest budget that could have included the target, when the target did not fit.
    ///
    /// A **floor, not a promise**: it is what the target costs plus what the answer already spent,
    /// and the report itself grows when it has more to say. The text says so too.
    pub minimum_for_target: Option<u64>,
    /// Whether this build chose the budget rather than being given one.
    pub budget_was_chosen: bool,
    /// Whether the budget was refused outright rather than answered. Never set: a budget the
    /// engine refused outright is a [`crate::exit::Failure`], and it reaches the caller as
    /// [`Answer::Declined`] rather than as a `ContextAnswer` — there was no pack, so there is
    /// nothing here to report as empty.
    ///
    /// Set for the *other* refusal, the one the engine answers: a budget large enough to compile
    /// the report and too small for the target, which is a real pack with no units in it. That
    /// case has a budget, omissions and a floor, and this flag is how a consumer tells it from a
    /// pack that simply came out full.
    pub refused: bool,
    /// Statements the pack owes its reader, beside the engine's own.
    pub notes: Vec<String>,
    /// What this command could not do.
    pub did_not: Vec<String>,
}

impl ContextAnswer {
    /// The human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        text.push_str(&self.pack.render(true));
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "candidates: {} examined, {} not reached\n",
            self.pack.budget.candidates_considered, self.pack.budget.candidates_unexamined
        ));
        for note in &self.pack.notes {
            text.push_str(&format!("note: {note}\n"));
        }
        for note in &self.notes {
            text.push_str(&format!("note: {note}\n"));
        }
        if let Some(floor) = self.minimum_for_target {
            text.push_str(&format!(
                "the target itself did not fit; at least {floor} token(s) would have been needed, \
                 and the report grows when it has more to say, so treat that as a floor\n"
            ));
        }
        text.push_str(&section("could not:", &self.did_not));
        text
    }
}

/// One `doctor` finding, flattened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorFinding {
    /// Which check produced it, by name.
    pub check: String,
    /// `pass`, `notice`, `warn` or `fail`.
    pub severity: String,
    /// One line, written to be read first.
    pub summary: String,
    /// The measurement it was derived from, and what it means.
    pub detail: String,
    /// What the user can do about it, when there is something.
    pub action: Option<String>,
}

impl From<&Finding> for DoctorFinding {
    fn from(finding: &Finding) -> Self {
        Self {
            check: check_name(finding.check),
            severity: finding.severity.as_str().to_owned(),
            summary: finding.summary.clone(),
            detail: finding.detail.clone(),
            action: finding.action.clone(),
        }
    }
}

/// `Check::as_str`, spelled out so a caller does not need the import.
fn check_name(check: Check) -> String {
    check.as_str().to_owned()
}

/// A diagnosis, with the counts the store measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorAnswer {
    /// Whether an index existed before this command ran.
    ///
    /// `peek_core::store::Store::open` creates a store when the file is absent, so a diagnosis of a
    /// repository that was never indexed describes an index this command made. The user needs to
    /// know that, and so does an agent.
    pub index_existed: bool,
    /// Whether nothing failed. Note that an *empty* index is healthy: the checks ran and passed.
    /// Whether the install is usable is the separate question of whether it was ever built.
    pub healthy: bool,
    /// The worst severity present, or `None` when nothing was checked.
    pub worst: Option<String>,
    /// Every finding, worst first.
    pub findings: Vec<DoctorFinding>,
    /// The store's own counts, when they could be read.
    pub counts: Option<Counts>,
    /// What this diagnosis could not do.
    pub did_not: Vec<String>,
}

impl DoctorAnswer {
    /// The human rendering: the engine's own report, then what it could not do.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = self.report();
        text.push_str(&section("\ncould not:", &self.did_not));
        text
    }

    /// `peek_core::doctor::Diagnosis::report`, rebuilt from the flattened findings.
    ///
    /// Reimplemented over the same values rather than copied, so the human text and the JSON are
    /// two renderings of one value. The ordering, the four-space detail indent and the closing
    /// `worst:` line are the module's own format and are reproduced exactly, because a golden file
    /// is only worth having if the thing it pins does not move for cosmetic reasons.
    fn report(&self) -> String {
        let mut text = String::new();
        for finding in &self.findings {
            text.push_str(&format!("[{}] {}\n", finding.severity, finding.summary));
            text.push_str(&format!("        {}\n", finding.detail));
            if let Some(action) = &finding.action {
                text.push_str(&format!("        try: {action}\n"));
            }
        }
        let worst = self
            .findings
            .iter()
            .filter(|finding| finding.severity != Severity::Pass.as_str())
            .max_by_key(|finding| severity_rank(&finding.severity));
        match worst {
            Some(finding) => text.push_str(&format!("\nworst: {}", finding.summary)),
            None => text.push_str("\nworst: nothing was checked"),
        }
        text
    }
}

/// The ordinal of a `doctor` severity label, for the worst-first ordering.
///
/// The four labels are the engine's own and their order is its enum's declaration order, so this is
/// a lookup in a four-row table rather than a second opinion about what "worse" means. **One**
/// definition, used both where the findings are ordered and where the report prints them, so the
/// order a reader sees and the order the JSON carries cannot come from two tables.
#[must_use]
pub fn severity_rank(label: &str) -> u8 {
    match label {
        "pass" => 0,
        "notice" => 1,
        "warn" => 2,
        "fail" => 3,
        // An unrecognised label cannot be ranked. Zero puts it with the passing findings rather than
        // at the top, because a severity this build does not know about is not evidence of a
        // problem; the findings around it still say what they found.
        _ => 0,
    }
}

/// The store's own counts.
///
/// One definition, used by `status` and by `doctor`, so the two commands cannot disagree about a
/// number. Every field is a `COUNT` or a `stat` taken by the store; none is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// The commit counter.
    pub generation: u64,
    /// The schema these rows were written with.
    pub schema_version: u32,
    /// Rows in `entity`.
    pub entity_count: u64,
    /// Rows in `relation`, of every state.
    pub relation_count: u64,
    /// Proven to have exactly one target.
    pub resolved_relations: u64,
    /// More than one candidate and no decisive evidence.
    pub ambiguous_relations: u64,
    /// Known to have no target, with a stated reason.
    pub unresolved_relations: u64,
    /// Derived from another fact rather than observed.
    pub inferred_relations: u64,
    /// Written but not yet decided.
    pub pending_relations: u64,
    /// Rows in `relation_candidate`: the total ambiguity held.
    pub candidate_count: u64,
    /// Relations naming a target that is not in the index.
    pub orphan_relations: u64,
    /// The main database file's size on disk.
    pub file_size_bytes: u64,
    /// The write-ahead log's size.
    pub wal_size_bytes: u64,
}

impl Counts {
    /// Copy the store's measurements, without recomputing any of them.
    #[must_use]
    pub fn from_stats(stats: &StoreStats) -> Self {
        Self {
            generation: stats.generation,
            schema_version: stats.schema_version,
            entity_count: stats.entity_count,
            relation_count: stats.relation_count,
            resolved_relations: stats.resolved_relations,
            ambiguous_relations: stats.ambiguous_relations,
            unresolved_relations: stats.unresolved_relations,
            inferred_relations: stats.inferred_relations,
            pending_relations: stats.pending_relations,
            candidate_count: stats.candidate_count,
            orphan_relations: stats.orphan_relations,
            file_size_bytes: stats.file_size_bytes,
            wal_size_bytes: stats.wal_size_bytes,
        }
    }

    /// Whether the five resolution states account for every relation in the index.
    ///
    /// Asserted rather than printed, because a set of counts that does not partition cannot be
    /// checked by a reader: "12,301 resolved and 879 inferred" out of 37,866 relations is a
    /// sentence with a number missing, and nothing in it says so. This project shipped exactly
    /// that once.
    #[must_use]
    pub fn states_partition(&self) -> bool {
        self.resolved_relations
            + self.ambiguous_relations
            + self.unresolved_relations
            + self.inferred_relations
            + self.pending_relations
            == self.relation_count
    }
}

/// The index's own numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusAnswer {
    /// The store's measurements.
    pub counts: Counts,
    /// The commit counter, repeated at the top because it is the number a caller compares.
    pub generation: u64,
    /// `FULL` or `NORMAL`, read back from the connection rather than assumed from a constant.
    pub durability: String,
    /// Whether the five states account for every relation.
    pub states_partition: bool,
    /// What this command could not do.
    pub did_not: Vec<String>,
}

impl StatusAnswer {
    /// The human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        let counts = &self.counts;
        let mut text = format!(
            "peek status\n\
             generation:  {}\n\
             schema:      v{}\n\
             durability:  {}\n\
             entities:    {}\n\
             relations:   {}\n\
             states:      {} resolved, {} inferred, {} ambiguous, {} unresolved, {} pending\n\
             candidates:  {} stored across the ambiguous relations\n\
             orphans:     {} relation(s) point at a target that is not in the index\n\
             database:    {} bytes, write-ahead log {} bytes\n",
            self.generation,
            counts.schema_version,
            self.durability,
            counts.entity_count,
            counts.relation_count,
            counts.resolved_relations,
            counts.inferred_relations,
            counts.ambiguous_relations,
            counts.unresolved_relations,
            counts.pending_relations,
            counts.candidate_count,
            counts.orphan_relations,
            counts.file_size_bytes,
            counts.wal_size_bytes
        );
        if !self.states_partition {
            text.push_str(&format!(
                "states do not add up: {} counted against {} relations; this index's own counts \
                 are inconsistent and cannot be checked\n",
                counts.resolved_relations
                    + counts.ambiguous_relations
                    + counts.unresolved_relations
                    + counts.inferred_relations
                    + counts.pending_relations,
                counts.relation_count
            ));
        }
        text.push_str(&section("could not:", &self.did_not));
        text
    }
}

/// What a removal took out of the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveAnswer {
    /// The path as the index knows it.
    pub path: String,
    /// `file` or `subtree`, decided by whether a directory is there now.
    pub scope: String,
    /// How many indexed files the removal covered, measured before the write.
    pub indexed_paths_removed: u64,
    /// Entity rows the store deleted.
    pub entities_removed: u64,
    /// Relation rows the store deleted, whether by cascade or directly.
    pub relations_removed: u64,
    /// The generation after the commit.
    pub generation: u64,
    /// Statements the removal owes its reader.
    pub notes: Vec<String>,
    /// What this command could not do.
    pub did_not: Vec<String>,
}

impl RemoveAnswer {
    /// The human rendering.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "peek rm\n\
             removed:    {path} ({scope})\n\
             indexed:    {paths} indexed file(s) at or below that path\n\
             rows:       {entities} entit(ies), {relations} relation(s) deleted\n\
             generation: {generation}\n",
            path = self.path,
            scope = self.scope,
            paths = self.indexed_paths_removed,
            entities = self.entities_removed,
            relations = self.relations_removed,
            generation = self.generation
        );
        if self.indexed_paths_removed == 0 {
            text.push_str(
                "            the index held nothing at that path, so nothing was removed; this is \
                 a measured zero and not an unperformed check\n",
            );
        }
        for note in &self.notes {
            text.push_str(&format!("note: {note}\n"));
        }
        text.push_str(&section("could not:", &self.did_not));
        text
    }
}

/// A command that did not answer, and the reason it did not.
///
/// **The whole of the answer is the reason.** There is no pack, no count and no report behind
/// this, and the type does not pretend otherwise: a caller that gets one has been told the truth
/// about a command that ran and declined, rather than handed a document about a different command.
///
/// The status is carried as well as on the envelope, and it is the *same* value: the exit code
/// beside it is derived from the one here, so the code a caller branches on and the reason it reads
/// cannot come from two decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclinedAnswer {
    /// The command that was asked for, as typed.
    ///
    /// Serialised as `command_name` because the enclosing variant's tag is already `command` and
    /// carries the answer's *kind*; the same split [`WalkAnswer::command`] uses, and for the same
    /// reason.
    #[serde(rename = "command_name")]
    pub command: String,
    /// The status, and therefore the exit code.
    pub status: Status,
    /// What went wrong, and what the caller can do about it. **The answer.**
    pub refusal: Refusal,
    /// What this command could not do. **Never empty**, and built from the status rather than
    /// left as an empty list: see the module documentation for why an empty one would read as a
    /// complete answer that happened to be short.
    pub did_not: Vec<String>,
}

impl From<&Failure> for DeclinedAnswer {
    fn from(failure: &Failure) -> Self {
        Self {
            command: failure.command.clone(),
            status: failure.status,
            refusal: failure.refusal.clone(),
            did_not: did_not_for(failure),
        }
    }
}

impl DeclinedAnswer {
    /// The human rendering: the status, the refusal, and what was not done.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "peek {command}: could not answer\n\
             status:     {status}\n\
             refusal:    {kind}\n\
             {reason}",
            command = self.command,
            status = self.status.as_str(),
            kind = self.refusal.kind,
            reason = self.refusal.render()
        );
        text.push_str(&section("\ncould not:", &self.did_not));
        text
    }
}

/// What a command that did not answer can honestly say it could not do.
///
/// **Never empty, and derived from the status.** An empty list on a failure would be
/// indistinguishable from a complete answer that happened to be short, which is the confusion the
/// whole `did_not` convention exists to prevent. Which statement is *true* depends on why the
/// command declined — a bad command line did not reach the repository, and a refusal did not answer
/// any part of the question — so it is a match on the status rather than one sentence stretched
/// over all five.
///
/// The `Status::Ok` arm is unreachable through the one function that builds this, which exists
/// precisely because a command that produced an answer never failed. It is written out rather than
/// folded into another arm so that a caller who constructs one by hand gets a statement telling
/// them it is a defect in `peek` rather than a result.
fn did_not_for(failure: &Failure) -> Vec<String> {
    let mut did_not = vec![format!(
        "this command did not answer the question it was asked, and there is no partial answer \
         behind it: the `{}` refusal above is the whole result",
        failure.refusal.kind
    )];
    did_not.push(match failure.status {
        Status::Ok => "this carries a successful status with no answer, which is a defect in peek \
                       rather than a result: there is nothing here to read"
            .to_owned(),
        Status::Usage => "the command line was not understood, so nothing was attempted against \
                          the repository. This exit code is about the invocation, not about the \
                          index"
            .to_owned(),
        Status::Refused => "the question cannot be answered from this index as it stands, and no \
                            part of it was answered. The same command against the same index is \
                            refused the same way"
            .to_owned(),
        Status::Unhealthy => "the diagnosis found something wrong and did not repair it. This \
                             command reports; it does not fix"
            .to_owned(),
        Status::Failed => "the engine stopped rather than reporting a partial answer, so no number \
                           here describes a half-finished state"
            .to_owned(),
    });
    did_not
}

/// The budget arithmetic the engine does not carry: the floor for including the target.
///
/// `spent` is the whole answer's cost, report included. A target that did not fit cost whatever
/// the engine priced it at, and the engine recorded that cost on the omission, so the floor is
/// `spent + that cost` — the point at which the same pack would have had room. It is a floor and
/// not a guarantee, because the report text itself grows when it has more to say.
#[must_use]
pub fn floor_for_target(pack: &ContextPack) -> Option<u64> {
    let target = pack.target.id().display();
    pack.omitted
        .iter()
        .find(|entry| entry.what == peek_core::query::Omitted::Unit && entry.subject == target)
        .map(|entry| pack.budget.spent_tokens.saturating_add(entry.cost.tokens))
}

/// A number of milliseconds, saturating rather than wrapping.
#[must_use]
pub fn millis(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The report's own partition check, for a run that did not build an `IndexAnswer`.
#[must_use]
pub fn report_partitions(report: &IndexReport) -> bool {
    report.relations_accounted_for() == report.relations_written
}
