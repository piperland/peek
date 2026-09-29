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

use peek_core::discover::DiscoveryOptions;
use peek_core::indexer::{self, IndexOutcome, IndexReport};
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
    session.refuse_if_watching("a refresh")?;
    let root = session.root().to_path_buf();
    let absolute: Vec<PathBuf> = paths.iter().map(|path| root.join(path)).collect();
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
        "advice": if partition { Value::Null } else { Value::String("run `doctor`; a state that does not partition means the index was written by something that does not agree with this build") },
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

/// The five resolution states, and whether they are a partition of what was written.
///
/// All five are always present. A reader who has to notice a missing key to know a state is absent
/// is a reader who will not notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolutionStates {
    /// Proven: exactly one target, with evidence that supports it.
    pub resolved: u64,
    /// Extracted but not yet decided. Zero after a successful pass; non-zero means work remains.
    pub pending: u64,
    /// More than one candidate fits and none was proven correct.
    pub ambiguous: u64,
    /// No target could be established, each with a stated reason.
    pub unresolved: u64,
    /// Derived from another fact, with the derivation recorded in the edge.
    pub inferred: u64,
}

impl ResolutionStates {
    /// The counts an index report produced.
    #[must_use]
    pub fn of(report: &IndexReport) -> Self {
        Self {
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
        text.push_str(&format!(
            "\nresolution states: {} resolved, {} inferred, {} ambiguous, {} unresolved, {} pending",
            self.resolution_states.resolved,
            self.resolution_states.inferred,
            self.resolution_states.ambiguous,
            self.resolution_states.unresolved,
            self.resolution_states.pending
        ));
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
