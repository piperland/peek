//! Diagnosing an install, so that "my index looks wrong" becomes an answer.
//!
//! # Why this is a module and not a command
//!
//! Because a command that shells out and prints prose cannot be tested, and a diagnosis that
//! cannot be tested is a diagnosis that drifts. Everything here is a function from a [`Store`] to
//! a list of [`Finding`]s, each with a severity, an explanation, and — where one exists — the
//! action that would fix it. The CLI is then a formatter over this, and the MCP surface is
//! another.
//!
//! # The rule that shapes it
//!
//! **A check that cannot be performed reports that it could not be performed.** It does not pass.
//! A green `doctor` that silently skipped the check it could not run is worse than no `doctor`,
//! because it converts an unknown into an assurance. This is the same rule the rest of the
//! project follows: a number that cannot be measured is not reported, and a guarantee that cannot
//! be established is not claimed.
//!
//! # What `doctor` is for
//!
//! Contract L1: *diagnose a deliberately broken install*. Every check below corresponds to a way
//! this index can be wrong in a way a user would notice and could not otherwise explain:
//!
//! | Symptom a user would report | The check that explains it |
//! |---|---|
//! | "it says no symbols for my file" | [`Check::PendingWork`], [`Check::UnsupportedFiles`] |
//! | "the index is from before I changed something" | [`Check::SchemaVersion`] |
//! | "it is slow" | [`Check::WalSize`], [`Check::Ambiguity`] |
//! | "is my data safe if the power goes out" | [`Check::Durability`] |
//! | "where is it storing my code" | [`Check::IndexLocation`] |
//! | "an edge points at nothing" | [`Check::OrphanEdges`] |
//!
//! The predecessor reported a healthy install over a corrupt index (audit A6) and turned a
//! failing `dir_size` into a plausible `0` (audit A16). Both are the failure of a health check
//! that reports a conclusion rather than an observation, and both are why every finding here
//! carries the measurement it was derived from.

use std::path::Path;

use crate::discover::{DiscoveryOptions, FileDiscovery, WalkIssueReason};
use crate::store::paths;
use crate::store::{Durability, SCHEMA_VERSION, Store, StoreError, StoreStats};

/// How much a finding should worry the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The check ran and the thing it checks is right.
    ///
    /// Reported rather than filtered out. Someone who runs `doctor` on a healthy install is
    /// asking what was checked, and an empty answer teaches them nothing about what the tool
    /// actually looked at.
    Pass,
    /// The index is usable and something about it is worth knowing.
    Notice,
    /// The index works but something is costing the user, and there is a way to fix it.
    Warn,
    /// The index cannot be trusted, or a required check could not be run.
    Fail,
}

impl Severity {
    /// The single word printed in a status column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Pass => "pass",
            Severity::Notice => "notice",
            Severity::Warn => "warn",
            Severity::Fail => "fail",
        }
    }
}

/// Which check produced a finding.
///
/// A named check rather than a free string, so `doctor --only` can select one and a test can
/// assert on a specific finding without matching prose that is free to be reworded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Check {
    /// The index could not be opened, or is not a Peek index.
    IndexOpenable,
    /// The stored shape is one this build understands.
    SchemaVersion,
    /// SQLite's own page and index consistency check.
    Integrity,
    /// The generation counter is consistent with the rows it claims to cover.
    Generation,
    /// Relations naming a target that is not in the index.
    OrphanEdges,
    /// Relations the resolver has not decided yet.
    PendingWork,
    /// Relations with more than one candidate, which is uncertainty stored rather than lost.
    Ambiguity,
    /// Size of the write-ahead log relative to the database.
    WalSize,
    /// Where the index lives, and that it is not inside the repository.
    IndexLocation,
    /// What durability the connection was opened with.
    Durability,
    /// What the walk would refuse, and why.
    UnsupportedFiles,
    /// Which repository this index belongs to.
    RepositoryIdentity,
}

impl Check {
    /// A stable, machine-readable name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Check::IndexOpenable => "index_openable",
            Check::SchemaVersion => "schema_version",
            Check::Integrity => "integrity",
            Check::Generation => "generation",
            Check::OrphanEdges => "orphan_edges",
            Check::PendingWork => "pending_work",
            Check::Ambiguity => "ambiguity",
            Check::WalSize => "wal_size",
            Check::IndexLocation => "index_location",
            Check::Durability => "durability",
            Check::UnsupportedFiles => "unsupported_files",
            Check::RepositoryIdentity => "repository_identity",
        }
    }
}

/// One thing `doctor` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub check: Check,
    pub severity: Severity,
    /// One line, written to be read first.
    pub summary: String,
    /// The measurement the summary was derived from, and what it means.
    ///
    /// Present even on a pass. A check that says "ok" without saying what it measured is
    /// indistinguishable from a check that did not run.
    pub detail: String,
    /// What the user can do about it, when there is something.
    pub action: Option<String>,
}

impl Finding {
    fn pass(check: Check, summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            check,
            severity: Severity::Pass,
            summary: summary.into(),
            detail: detail.into(),
            action: None,
        }
    }

    fn problem(
        check: Check,
        severity: Severity,
        summary: impl Into<String>,
        detail: impl Into<String>,
        action: Option<String>,
    ) -> Self {
        Self {
            check,
            severity,
            summary: summary.into(),
            detail: detail.into(),
            action,
        }
    }
}

/// The whole diagnosis.
#[derive(Debug, Clone, Default)]
pub struct Diagnosis {
    pub findings: Vec<Finding>,
    /// The store's own measurements, so a formatter need not re-query.
    pub stats: Option<StoreStats>,
}

impl Diagnosis {
    /// The worst severity present, which is the one a status line should show.
    ///
    /// `None` for an empty diagnosis rather than a defaulting `Pass`, because "nothing was
    /// checked" and "everything is fine" are different facts and the type should not be able to
    /// confuse them.
    pub fn worst(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }

    /// Whether anything failed. A `doctor` that exits zero here is lying about its own input.
    pub fn is_healthy(&self) -> bool {
        self.worst().is_some_and(|worst| worst < Severity::Fail)
    }

    /// The findings for one check, in order.
    pub fn check(&self, check: Check) -> Vec<&Finding> {
        self.findings.iter().filter(|f| f.check == check).collect()
    }

    /// The first finding at or above `severity`, if any. What a formatter prints when it wants
    /// the one line that matters.
    pub fn worst_finding(&self) -> Option<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity > Severity::Pass)
            .max_by_key(|f| f.severity)
    }

    /// A multi-line report, for a terminal.
    ///
    /// The worst finding comes first, because a user who reads one line reads the first one.
    pub fn report(&self) -> String {
        let mut ordered: Vec<&Finding> = self.findings.iter().collect();
        // Stable within a severity, so two runs over an unchanged index print the same report.
        // `sort_by_key` is the tool for this and says so; the comparison above spelled out what it
        // does, which is not a virtue in a sort.
        ordered.sort_by_key(|finding| std::cmp::Reverse(finding.severity));
        let mut out = String::new();
        for finding in ordered {
            out.push_str(&format!("[{}] {}\n", finding.severity.as_str(), finding.summary));
            out.push_str(&format!("        {}\n", finding.detail));
            if let Some(action) = &finding.action {
                out.push_str(&format!("        try: {action}\n"));
            }
        }
        match self.worst_finding() {
            Some(worst) => out.push_str(&format!("\nworst: {}", worst.summary)),
            None => out.push_str("\nworst: nothing was checked"),
        }
        out
    }
}

/// Diagnose the index for the repository at `root`.
///
/// `root` is the repository, not the index: the location of the index is one of the things being
/// diagnosed, so taking it as a parameter would let a caller accidentally check the wrong one and
/// never find out.
pub fn diagnose(root: &Path) -> Diagnosis {
    match open_for_diagnosis(root) {
        Ok((store, repo)) => diagnose_open(&store, root, &repo),
        Err(error) => Diagnosis {
            findings: vec![Finding::problem(
                Check::IndexOpenable,
                Severity::Fail,
                "the index could not be opened",
                format!("{error}"),
                Some(
                    "if the index is from an older or newer Peek, delete it and re-index; the \
                     source is the only thing that cannot be rebuilt"
                        .to_owned(),
                ),
            )],
            stats: None,
        },
    }
}

/// Open the index, returning the store and the identity it was opened against.
fn open_for_diagnosis(root: &Path) -> Result<(Store, crate::store::RepoId), StoreError> {
    let repo = crate::store::RepoId::discover(root)?;
    let path = paths::index_path(&repo)?;
    let store = Store::open(&path, &repo)?;
    Ok((store, repo))
}

/// Diagnose an already-open store. Separate from [`diagnose`] so a caller that has a store — a
/// daemon, a test — does not have to close and reopen it to ask.
pub fn diagnose_open(store: &Store, root: &Path, repo: &crate::store::RepoId) -> Diagnosis {
    let mut findings = Vec::new();

    findings.push(check_index_openable(store));
    findings.push(check_schema_version(store));
    findings.push(check_integrity(store));
    findings.push(check_repository_identity(store, repo, root));
    findings.push(check_index_location(store, root));
    findings.push(check_durability(store));

    // Every remaining check reads the store's own counts, so one failure to gather them is
    // reported once rather than as a dozen identical failures.
    match store.stats() {
        Ok(stats) => {
            findings.push(check_generation(&stats));
            findings.push(check_orphan_edges(&stats));
            findings.push(check_pending_work(&stats));
            findings.push(check_ambiguity(&stats));
            findings.push(check_wal_size(&stats));
            Diagnosis {
                findings,
                stats: Some(stats),
            }
        }
        Err(error) => {
            findings.push(Finding::problem(
                Check::Generation,
                Severity::Fail,
                "the index's own counts could not be read",
                format!("{error}"),
                Some("run `doctor` again; if it persists the index is unreadable".to_owned()),
            ));
            Diagnosis {
                findings,
                stats: None,
            }
        }
    }
    .with_walk(root)
}

impl Diagnosis {
    /// Append the checks that need a filesystem walk rather than the store.
    fn with_walk(mut self, root: &Path) -> Self {
        self.findings.push(check_unsupported_files(root));
        self
    }
}

fn check_index_openable(store: &Store) -> Finding {
    Finding::pass(
        Check::IndexOpenable,
        format!("the index at {} opened", store.path().display()),
        format!(
            "generation {}, durability {}, WAL-backed, foreign keys enforced",
            store.generation(),
            store.durability().as_str()
        ),
    )
}

fn check_schema_version(store: &Store) -> Finding {
    let stored = store.schema_version();
    if stored == SCHEMA_VERSION {
        Finding::pass(
            Check::SchemaVersion,
            format!("the index is at schema v{stored}"),
            format!("this build writes and reads v{SCHEMA_VERSION}"),
        )
    } else {
        Finding::problem(
            Check::SchemaVersion,
            Severity::Fail,
            format!("the index is at schema v{stored}, this build is at v{SCHEMA_VERSION}"),
            "a store written by a different shape of Peek cannot be read reliably, and guessing \
             what its rows meant is how a confident wrong answer is produced"
                .to_owned(),
            Some("delete the index and re-index from source".to_owned()),
        )
    }
}

fn check_integrity(store: &Store) -> Finding {
    match store.verify() {
        Ok(()) => Finding::pass(
            Check::Integrity,
            "the index passes SQLite's own integrity check",
            "every page, and every index against the table it indexes, was read and compared. This \
             is the check the predecessor had no equivalent of: it reported a healthy install over \
             a broken index"
                .to_owned(),
        ),
        Err(error) => Finding::problem(
            Check::Integrity,
            Severity::Fail,
            "the index is damaged",
            format!("{error}"),
            Some(
                "delete the index and re-index from source; a damaged index cannot be repaired \
                 in place without risking a plausible answer from a broken row"
                    .to_owned(),
            ),
        ),
    }
}

fn check_repository_identity(store: &Store, repo: &crate::store::RepoId, root: &Path) -> Finding {
    let matches = store.repo() == repo;
    let detail = format!(
        "identity {} for {}, which is {} so two worktrees cannot share one index",
        store.repo().as_str(),
        root.display(),
        if matches { "correct" } else { "MISMATCHED" }
    );
    if matches {
        Finding::pass(
            Check::RepositoryIdentity,
            format!("the index belongs to this checkout ({})", store.repo().as_str()),
            detail,
        )
    } else {
        Finding::problem(
            Check::RepositoryIdentity,
            Severity::Fail,
            "the index belongs to a different checkout",
            detail,
            Some(
                "this is a cache-key collision or a copied index; delete it and re-index"
                    .to_owned(),
            ),
        )
    }
}

fn check_index_location(store: &Store, root: &Path) -> Finding {
    let path = store.path();
    let inside = paths::is_within(path, root);
    let detail = format!("the index lives at {}", path.display());
    if inside {
        Finding::problem(
            Check::IndexLocation,
            Severity::Warn,
            "the index is inside the repository it describes",
            format!(
                "{detail}, which means it is committed, cloned and copied between machines by \
                 accident. It is a cache and belongs in the OS cache directory"
            ),
            Some("set PEEK_INDEX_DIR, delete the in-repository index, and re-index".to_owned()),
        )
    } else {
        Finding::pass(
            Check::IndexLocation,
            "the index is outside the repository",
            detail,
        )
    }
}

fn check_durability(store: &Store) -> Finding {
    match store.durability() {
        Durability::Full => Finding::pass(
            Check::Durability,
            "commits are durable when they return",
            "synchronous=FULL, so a commit that returned success is on disk. A power failure \
             cannot lose a generation that was reported as written"
                .to_owned(),
        ),
        Durability::Normal => Finding::problem(
            Check::Durability,
            Severity::Warn,
            "commits may be lost on power failure",
            "synchronous=NORMAL. Atomicity still holds, so the index is never torn, but a \
             transaction that reported success can be absent after power loss"
                .to_owned(),
            Some("reopen the index with the default durability".to_owned()),
        ),
    }
}

fn check_generation(stats: &StoreStats) -> Finding {
    // A store that has rows but has never committed is internally inconsistent: every write
    // advances the generation, so rows with generation 0 mean something wrote around the write
    // path. It is the cheapest possible evidence of exactly the defect this project exists to
    // prevent, so it is worth a check of its own.
    let has_rows = stats.entity_count > 0 || stats.relation_count > 0;
    let never_committed = stats.generation == 0;
    if has_rows && never_committed {
        return Finding::problem(
            Check::Generation,
            Severity::Fail,
            "the index holds rows but has never committed",
            format!(
                "{} entities and {} relations with generation 0. Every write advances the \
                 generation, so these rows were not written through the write path",
                stats.entity_count, stats.relation_count
            ),
            Some("delete the index and re-index from source".to_owned()),
        );
    }
    if !has_rows {
        return Finding::problem(
            Check::Generation,
            Severity::Notice,
            "the index is empty",
            format!("generation {}, no entities, no relations", stats.generation),
            Some("run `peek index` to build it".to_owned()),
        );
    }
    Finding::pass(
        Check::Generation,
        format!("generation {} covers {} relations", stats.generation, stats.relation_count),
        format!(
            "{} entities and {} relations, and the generation is non-zero, so the write path \
             produced them",
            stats.entity_count, stats.relation_count
        ),
    )
}

fn check_orphan_edges(stats: &StoreStats) -> Finding {
    if stats.orphan_relations > 0 {
        return Finding::problem(
            Check::OrphanEdges,
            Severity::Fail,
            format!("{} relations point at something not in the index", stats.orphan_relations),
            "a relation whose target is missing is a confident answer to a question about code \
             that is not there. Foreign keys make this structurally impossible, so a non-zero \
                 count means something wrote to the database without going through Peek"
                .to_owned(),
            Some("delete the index and re-index from source".to_owned()),
        );
    }
    Finding::pass(
        Check::OrphanEdges,
        "no relation points at a target that is missing",
        "checked by an anti-join against the entity table, not assumed from the foreign key",
    )
}

fn check_pending_work(stats: &StoreStats) -> Finding {
    if stats.pending_relations > 0 {
        return Finding::problem(
            Check::PendingWork,
            Severity::Notice,
            format!("{} relations have not been decided", stats.pending_relations),
            format!(
                "{} of {} relations — these are extracted references the resolver has not placed \
                 yet. They are stored and countable rather than dropped, which is the point, but \
                 a query about them cannot be answered",
                stats.pending_relations, stats.relation_count
            ),
            Some("re-run the index so the resolution pass runs".to_owned()),
        );
    }
    Finding::pass(
        Check::PendingWork,
        "every relation has been decided",
        "no relation is waiting on the resolver",
    )
}

fn check_ambiguity(stats: &StoreStats) -> Finding {
    let total = stats.relation_count;
    if stats.ambiguous_relations == 0 {
        return Finding::pass(
            Check::Ambiguity,
            "no relation is ambiguous",
            "every decided edge names exactly one target",
        );
    }
    // `checked_div` rather than a guard, so the zero case is impossible to get wrong by
    // accident: an empty relation table has no percentage.
    let percent = stats
        .ambiguous_relations
        .checked_mul(100)
        .and_then(|scaled| scaled.checked_div(total))
        .unwrap_or(0);
    // Ambiguity is not a defect, so this is a notice and never a warning: it is a fact about the
    // codebase that a consumer needs to be able to see. A tool that reported it as a problem
    // would be training the user to ignore the field that tells them where the graph is thin.
    let severity = if percent >= 50 {
        Severity::Warn
    } else {
        Severity::Notice
    };
    Finding::problem(
        Check::Ambiguity,
        severity,
        format!(
            "{} relations ({percent}%) have more than one candidate",
            stats.ambiguous_relations
        ),
        format!(
            "{} candidates are stored across them, ordered by evidence strength. These are not \
             guesses: the relation is reported as undecided and its candidates are retrievable. A \
             high share means the repository has many same-named symbols, which is a fact about \
             the code rather than about this index",
            stats.candidate_count
        ),
        Some(
            "adding module structure to the index disambiguates by scope; until then these are \
             honestly undecided"
                .to_owned(),
        ),
    )
}

fn check_wal_size(stats: &StoreStats) -> Finding {
    // A log as large as the database means a second full copy of the index on disk. It is a
    // symptom with a cause, and the cause is usually that nothing has checkpointed — which is
    // worth telling the user rather than silently fixing, because a caller that checkpoints on
    // every commit would turn a refresh into a disk storm.
    if stats.file_size_bytes > 0 && stats.wal_size_bytes * 2 > stats.file_size_bytes {
        return Finding::problem(
            Check::WalSize,
            Severity::Warn,
            format!(
                "the write-ahead log is {} bytes against a {} byte database",
                stats.wal_size_bytes, stats.file_size_bytes
            ),
            "the log holds committed transactions that have not been folded back into the \
             database, so the index currently costs about twice what it should on disk. A reader \
             holding a snapshot is what prevents the checkpoint, and that is not a fault"
                .to_owned(),
            Some("close other readers, or checkpoint when the watcher goes idle".to_owned()),
        );
    }
    Finding::pass(
        Check::WalSize,
        format!("the write-ahead log is {} bytes", stats.wal_size_bytes),
        format!("against a {} byte database", stats.file_size_bytes),
    )
}

fn check_unsupported_files(root: &Path) -> Finding {
    let discovery = match FileDiscovery::new(root, DiscoveryOptions::default()).discover() {
        Ok(discovery) => discovery,
        Err(error) => {
            return Finding::problem(
                Check::UnsupportedFiles,
                Severity::Fail,
                "the repository could not be walked, so this check did not run",
                format!("{error}"),
                Some("check the path and the permissions on it".to_owned()),
            );
        }
    };

    let stats = discovery.stats();
    let unsupported = stats.unsupported_extension;
    let not_utf8 = stats.not_utf8;
    let too_large = stats.too_large;
    let refused = unsupported + not_utf8 + too_large + stats.unreadable;

    if refused == 0 {
        return Finding::pass(
            Check::UnsupportedFiles,
            "every file in the repository was indexable",
            format!(
                "{} files yielded, none refused",
                stats.files_yielded
            ),
        );
    }

    let detail = format!(
        "{} of {} examined files were not indexed: {unsupported} with no language, {not_utf8} \
         not valid UTF-8, {too_large} over the size cap, {} unreadable. Files this tool could not \
         read are named in the walk report rather than dropped silently",
        refused, stats.files_examined, stats.unreadable
    );
    let action = if unsupported > 0 {
        Some(
            "a file with no language is a language Peek has no extraction rules for; the count is \
             the honest measure of that, and it is not the same as a file with no symbols"
                .to_owned(),
        )
    } else {
        Some("a file that cannot be decoded is skipped on purpose, never partially indexed".to_owned())
    };

    Finding::problem(
        Check::UnsupportedFiles,
        Severity::Notice,
        format!("{refused} files were not indexed"),
        detail,
        action,
    )
}

/// The names of the walk's refusal reasons, for a caller that wants to print them.
///
/// A public accessor rather than a hardcoded list, so a reason added to the walker is reported
/// here too. Returning the *set* rather than a count is the point: "12 files were not indexed" is
/// not actionable, and "3 of them are `.h` files" is.
pub fn refusal_reasons(root: &Path) -> Vec<(&'static str, u64)> {
    let Ok(discovery) = FileDiscovery::new(root, DiscoveryOptions::default()).discover() else {
        return Vec::new();
    };
    let mut counts: std::collections::BTreeMap<&'static str, u64> = Default::default();
    for issue in discovery.issues() {
        let name = match &issue.reason {
            WalkIssueReason::Unreadable { .. } => "unreadable",
            WalkIssueReason::OutsideRepository => "outside_repository",
            WalkIssueReason::NonUtf8Path => "non_utf8_path",
            WalkIssueReason::SymlinkEscapes { .. } => "symlink_escapes",
            WalkIssueReason::UnresolvableSymlink { .. } => "unresolvable_symlink",
            WalkIssueReason::Duplicate => "duplicate",
            WalkIssueReason::UnsupportedExtension { .. } => "unsupported_extension",
            WalkIssueReason::TooLarge { .. } => "too_large",
            WalkIssueReason::NotUtf8 { .. } => "not_utf8",
        };
        *counts.entry(name).or_default() += 1;
    }
    counts.into_iter().collect()
}

#[cfg(test)]
mod tests;
