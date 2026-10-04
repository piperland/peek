//! The indexer: the one operation that turns a repository into a queryable index.
//!
//! This is where discovery, extraction and storage meet, and it is the only place that can be
//! transactional across all three. It exists as its own module because that boundary is where
//! the interesting failures live.
//!
//! # What this fixes about the engine Peek replaces
//!
//! * **Storage errors were invisible.** Every persisted write went through one `.ok()`, so a full
//!   disk reported "indexed N files" with nothing on disk. Here a failure at any stage is an
//!   `Err`, and nothing is reported as success unless the transaction committed.
//! * **The revision counter was not a generation.** It reset to 1 on every full build, so it
//!   could not detect a stale or torn read. The store owns a monotonic generation and this module
//!   never fabricates one.
//! * **A refresh cost O(repository).** Every watch event rebuilt the whole graph and rewrote the
//!   whole store, making a branch switch quadratic. Here a refresh touches only the changed paths.
//! * **One bad file aborted everything.** A single non-UTF-8 file failed the whole run and
//!   persisted nothing. Here it degrades that one file and is reported.
//!
//! # Ordering
//!
//! Removals precede upserts inside one transaction, so a path that is removed and re-added in the
//! same batch ends up present. That is what lets a refresh replace a whole directory in one commit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::containment::relative_path;
use crate::discover::{
    DiscoveredFile, DiscoveryOptions, DiscoveryStats, FileDiscovery, WalkIssue, WalkIssueReason,
};
use crate::extract::ExtractedFile;
use crate::model::{EntityId, Language, Relation, RelationKey, RepoPath, ResolutionState};
use crate::resolve::{self, ResolutionOptions, ResolutionReport};
use crate::store::{IndexUpdate, RepoId, Store, StoreError, UpdateStats, paths};

/// How many entities of one file are read when collecting the edges a refresh is about to
/// displace, and how many edges each of them may contribute.
///
/// A bound, because a file with a thousand callers would otherwise make one refresh read a
/// thousand rows without limit. It is generous enough that the cap is not reached by ordinary
/// code, and it is a named constant rather than a literal so that raising it is a visible act.
const DISPLACED_EDGE_SCAN_LIMIT: usize = 2048;

/// Everything an indexing run produced, including what it could not do.
///
/// Every counter here is measured. A number that cannot be measured is not reported — which is
/// the defect this replaces, where a failing size calculation became a plausible `0`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// The store generation after the commit. Never fabricated.
    pub generation: u64,
    /// Files read and extracted successfully.
    pub files_indexed: u64,
    /// Files that could not be read or decoded. Counted, never fatal.
    pub files_skipped: u64,
    /// Files whose language has no extraction rules. **Not** the same as "no symbols".
    pub files_unsupported: u64,
    /// Files whose parse was degraded, so their extraction is best effort.
    pub files_degraded: u64,
    /// Paths removed from the index. **Paths**, not entities — one deleted file routinely removes
    /// a file entity plus every symbol in it, and reporting that as "2 files removed" is a lie a
    /// caller cannot check.
    pub files_removed: u64,
    pub entities_written: u64,
    /// Entity rows deleted, which is the store's `entities_removed` measured directly.
    pub entities_removed: u64,
    pub relations_written: u64,
    /// Relations the extractor left `Pending`, before the resolution pass ran.
    ///
    /// The extractor never decides anything, so this is the *input* to resolution rather than a
    /// result, and the result is [`Self::resolution`]. Reporting the input without the result
    /// would be a report that says only what work is outstanding.
    pub relations_pending: u64,
    /// Relations the extractor could not resolve at all, with a stated reason.
    pub relations_unresolved: u64,
    /// Relations the extractor found genuinely ambiguous. Candidates are retrievable in order.
    pub relations_ambiguous: u64,
    /// Relations the extractor bound to a target by observation.
    pub relations_resolved: u64,
    /// Relations bound to a target by inference rather than direct observation.
    pub relations_inferred: u64,
    /// Relations the index still holds awaiting a decision, measured after the resolution pass.
    ///
    /// **Zero is the only value a correct run produces.** It is measured from the store rather than
    /// taken from [`Self::relations_pending`], because those are two different populations: that one
    /// is what the extractor emitted, this one is what survived the pass.
    ///
    /// For a full build they are the same set — the store held nothing beforehand. For a refresh
    /// this is the whole index's outstanding total, which is zero unless an earlier run left work
    /// behind. Reporting the index's total is the deliberate choice: "this run decided everything
    /// it wrote" and "this index has nothing outstanding" are different claims, and only the second
    /// is one a reader of the index can rely on.
    pub relations_undecided: u64,
    /// Entities that are test cases.
    pub tests_found: u64,
    /// Size of the write-ahead log after the run, in bytes.
    ///
    /// Reported because it is routinely as large as the whole database when nothing has
    /// checkpointed it, which means a user pays for the index twice on disk. Measured after the
    /// run rather than assumed to be zero: a checkpoint refused by a concurrent reader leaves a
    /// real number here, and a number is something `doctor` can act on.
    pub wal_bytes: u64,
    /// What the second pass did: the decisions taken on the relations this run wrote, plus the
    /// ones that already existed and pointed into the files that changed.
    ///
    /// `None` only if the resolution pass was not run, which no current caller does. It is an
    /// `Option` because "a report without a resolution pass" and "a report whose resolution pass
    /// decided nothing" are different facts, and collapsing them would be the same conflation
    /// that made fourteen languages report as supported while extracting nothing.
    pub resolution: Option<ResolutionReport>,
    /// Wall-clock time for the run.
    pub elapsed: Duration,
}

impl IndexReport {
    /// A one-line summary for `peek status` and the MCP `index_status` primitive.
    pub fn summary(&self) -> String {
        // The two passes are separate commits with separate failures, so their results are
        // reported as separate clauses rather than interleaved. A reader who sees them mixed
        // cannot tell which stage made a decision.
        format!(
            "generation {}: {} files indexed ({} skipped, {} unsupported, {} degraded), \
             {} removed, {} entities, {} relations ({} resolved, {} pending, {} ambiguous, \
             {} unresolved, {} inferred), {} tests, wal {} bytes{}{}, {:?}",
            self.generation,
            self.files_indexed,
            self.files_skipped,
            self.files_unsupported,
            self.files_degraded,
            self.files_removed,
            self.entities_written,
            self.relations_written,
            self.relations_resolved,
            self.relations_pending,
            self.relations_ambiguous,
            self.relations_unresolved,
            self.relations_inferred,
            self.tests_found,
            self.wal_bytes,
            self.resolution
                .as_ref()
                .map_or(String::new(), |r| format!("; {}", r.summary())),
            // Named apart from the `pending` in the parenthesis above, because that one is the
            // extractor's output and this one is what the index is left holding. A summary that
            // printed only the first would report a clean run over an index that cannot answer
            // questions about its own edges, and the two differ exactly when a scoped pass did not
            // reach everything it was given.
            match self.relations_undecided {
                0 => String::new(),
                other => format!("; {other} left undecided"),
            },
            self.elapsed
        )
    }

    /// The five resolution states, added up.
    ///
    /// Provided so a caller — and a test — can check that the report accounts for every relation
    /// it claims to have written, rather than taking the summary's word for it.
    pub fn relations_accounted_for(&self) -> u64 {
        self.relations_resolved
            + self.relations_pending
            + self.relations_ambiguous
            + self.relations_unresolved
            + self.relations_inferred
    }
}

/// Why a file was not indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// No registered extraction rules for this file's extension.
    ///
    /// Reported separately from "no symbols found" on purpose. Conflating the two is how an
    /// engine advertises twenty-eight languages of which fourteen extract nothing. The extension
    /// is carried rather than the language, because discovery decides this *before* it knows a
    /// language — a file with no extension at all has no language either.
    UnsupportedExtension(Option<String>),
    /// The file's language is known but no extraction rules are registered for it.
    UnsupportedLanguage(Language),
    /// The bytes are not valid UTF-8.
    NotUtf8 {
        /// Where the invalid sequence starts, in bytes.
        offset: usize,
    },
    /// The file is larger than the configured cap.
    TooLarge { bytes: u64, cap: u64 },
    /// The file could not be read at all.
    Unreadable(String),
    /// The path is outside the repository root.
    OutsideRoot,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::UnsupportedExtension(None) => f.write_str("no file extension"),
            SkipReason::UnsupportedExtension(Some(extension)) => {
                write!(f, "no extraction rules for .{extension}")
            }
            SkipReason::UnsupportedLanguage(language) => {
                write!(f, "no extraction rules for {language}")
            }
            SkipReason::NotUtf8 { offset } => {
                write!(f, "not valid UTF-8 (at byte {offset})")
            }
            SkipReason::TooLarge { bytes, cap } => {
                write!(f, "{bytes} bytes exceeds the {cap} byte cap")
            }
            SkipReason::Unreadable(detail) => write!(f, "unreadable: {detail}"),
            SkipReason::OutsideRoot => f.write_str("outside the repository root"),
        }
    }
}

/// A file that was considered and not indexed, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
}

/// What an indexing run did, including what it refused to do.
#[derive(Debug, Clone, Default)]
pub struct IndexOutcome {
    pub report: IndexReport,
    pub skipped: Vec<SkippedFile>,
}

impl IndexOutcome {
    /// The report alone.
    pub fn report(&self) -> &IndexReport {
        &self.report
    }
}

/// Errors an indexing run can produce. Every one is terminal for that run.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("discovery failed: {0}")]
    Discovery(#[from] crate::discover::DiscoveryError),
    #[error("the index store failed: {0}")]
    Store(#[from] StoreError),
}

/// Open the index for the repository at `root`, at its configured location.
///
/// The path is resolved rather than passed in, so that no caller can accidentally place the index
/// inside the tree it describes. Audit A: the engine this replaces wrote `.cortex/` into every
/// repository it touched and gitignored it — invisible to a reviewer, and carried by every clone.
/// See [`crate::store::paths`].
pub fn open_store(root: &Path) -> Result<Store, IndexError> {
    let repo = RepoId::discover(root)?;
    let path = paths::index_path(&repo)?;
    Ok(Store::open(&path, &repo)?)
}

/// Build a complete index of `root`.
///
/// Two transactions and one discovery walk. The first commits the extractor's output, the second
/// commits the resolver's decisions about it. A failure in either leaves the previous generation
/// intact and readable, which is the crash-safety requirement; a crash *between* them leaves a
/// complete index whose relations are honestly `Pending`, which a later pass can finish.
///
/// The split is the resolver's decision, argued in [`resolve`]. In short: resolution is a
/// function of the whole committed index, and it must be able to run over edges whose source file
/// did not change.
pub fn build_full(
    store: &mut Store,
    root: &Path,
    options: DiscoveryOptions,
) -> Result<IndexOutcome, IndexError> {
    let started = Instant::now();
    let discovery = FileDiscovery::new(root, options).discover()?;

    let mut outcome = IndexOutcome::default();
    // Discovery refused some files before this module ever saw them — non-UTF-8, oversized,
    // unreadable, unrecognised extension. Those counts belong in the report. Without this the
    // indexer would report a clean run over a tree where a third of the source was silently
    // absent, which is the exact failure this whole engine exists to remove.
    absorb_discovery(&mut outcome, discovery.report().stats(), discovery.issues());

    let mut update = IndexUpdate::empty();

    for file in discovery.files() {
        let absolute = discovery.report().absolute(&file.path);
        update = ingest(file, &absolute, update, &mut outcome);
    }

    let stats = store.apply_update(update)?;
    absorb(&mut outcome.report, &stats);
    outcome.report.generation = store.generation();
    // A full build leaves every one of its own relations `Pending`, and the second pass decides
    // all of them. `resolve_all` is used rather than a path-scoped pass because a full build has
    // no "changed" subset: everything is new, so there is nothing to scope to.
    let resolution = resolve::resolve_all(store, ResolutionOptions::default())?;
    outcome.report.generation = store.generation();
    outcome.report.resolution = Some(resolution);

    // Checkpoint **after** resolution, not before. Measured on `rust-lang/regex`: 227 files left a
    // write-ahead log of 30,479,792 bytes against a 30,199,808-byte database — a second full copy
    // of the index, on disk, until something replayed or truncated it. Nothing was calling
    // `checkpoint()`, so a user who indexed a repository and quit simply paid for it twice.
    //
    // A refused checkpoint is **not** a build failure: it means a reader holds a snapshot, the
    // committed data is still correct, and the next run will fold the log in. So the outcome is
    // recorded as a measured WAL size rather than as an error, and `.ok()`-swallowed. The
    // difference matters: the number is now visible, where before there was no signal at all.
    let _ = store.checkpoint();
    measure(&mut outcome.report, store);
    outcome.report.elapsed = started.elapsed();
    Ok(outcome)
}

/// Fill in the two numbers only the store can measure, from one pass over it.
///
/// One call rather than two because [`Store::stats`] costs a scan of both tables, and a refresh
/// runs it per keystroke under `watch`. Both fields are measured rather than inferred: the log size
/// because a refused checkpoint leaves a real one, and the undecided count because the whole point
/// is that it can disagree with the report's own pending figure.
fn measure(report: &mut IndexReport, store: &Store) {
    let Ok(stats) = store.stats() else {
        // Both fields stay zero. That is a real limitation rather than a measurement: the write has
        // already committed, so failing the run here would report an indexing failure on the
        // grounds of a diagnostic that did not take. A caller that needs these numbers reads the
        // store itself.
        return;
    };
    report.wal_bytes = stats.wal_size_bytes;
    report.relations_undecided = stats.pending_relations;
}

/// Refresh only `paths`, which is what `watch` calls.
///
/// Cost is proportional to the number of changed files, not to the size of the repository. A
/// path that no longer exists is removed; one that does is re-extracted. That is the entirety of
/// incremental indexing, and the reason a branch switch no longer costs a full rebuild.
/// The resolution pass afterwards is scoped to the same paths **and to the relations that point
/// into them**, which is the only way a refresh stays correct when a definition moves: the callers
/// of a function that changed file did not change, so re-extracting the changed files alone would
/// leave every one of their edges pointing at a target that is no longer there.
pub fn refresh(
    store: &mut Store,
    root: &Path,
    paths: &[PathBuf],
    options: &DiscoveryOptions,
) -> Result<IndexOutcome, IndexError> {
    let started = Instant::now();
    let mut outcome = IndexOutcome::default();
    let mut update = IndexUpdate::empty();
    let mut touched: Vec<RepoPath> = Vec::new();
    // The edges that are about to point at nothing. Read *before* the write, because the write
    // demotes them to a null target and they stop being findable by target afterwards.
    let mut displaced: Vec<Relation> = Vec::new();
    // The widest thing this batch wrote, so the pass below can be given a scope that reaches it.
    let mut scope = ScopeBounds::default();

    for path in paths {
        // **The repository-relative name, or nothing.** `strip_prefix` alone was not the question:
        // it says `Err` for a path outside the root, and the `unwrap_or` beside it handed the whole
        // path on — so an absolute path that was not under the root became the name the index held
        // it under. `RepoPath` accepts `/etc/passwd` as `etc/passwd`, so the store took a row
        // keyed by an absolute path on the host, and a caller that names paths (`index` in refresh
        // mode over MCP) could have the engine read a file outside the repository and index it.
        //
        // The rule is [`crate::containment`]'s lexical one, and lexical is the right one *here*:
        // the key has to be the key the discovery walk would give the same file, and discovery keys
        // a file by the path it was walked under without resolving a symlink first. Resolving here
        // would key a followed link by its target and make an incremental build disagree with a
        // full one about the same repository — which is the defect a refresh is supposed to be the
        // cheap version of.
        let inside = relative_path(root, path);
        let Some(relative) = inside.and_then(|name| RepoPath::from_path(&name)) else {
            outcome.report.files_skipped += 1;
            outcome.skipped.push(SkippedFile {
                path: path.display().to_string(),
                reason: SkipReason::OutsideRoot,
            });
            continue;
        };

        // Every relation arriving at an entity of this file is about to have its target nulled:
        // a refresh *always* removes a file's rows before re-inserting them, because an upsert
        // alone would leave rows for symbols that no longer exist. Capturing those edges here is
        // what lets the resolver decide them again afterwards, instead of every caller of a
        // symbol in this file being left pointing at nothing the moment it is edited.
        collect_incoming(store, &relative, &mut displaced)?;

        if !path.is_file() {
            update = update.removing_file(relative.clone());
            outcome.report.files_removed += 1;
            continue;
        }

        let Some(language) = relative
            .extension()
            .and_then(|ext| Language::from_extension(&ext))
        else {
            // No known language: the path cannot contribute entities, so drop any rows it once
            // had. Leaving them would be a stale index that reports symbols no longer on disk.
            update = update.removing_file(relative.clone());
            outcome.report.files_unsupported += 1;
            outcome.skipped.push(SkippedFile {
                path: relative.to_string(),
                reason: SkipReason::UnsupportedLanguage(Language::Bash),
            });
            continue;
        };

        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if bytes > options.max_file_bytes() {
            update = update.removing_file(relative.clone());
            outcome.report.files_skipped += 1;
            outcome.skipped.push(SkippedFile {
                path: relative.to_string(),
                reason: SkipReason::TooLarge {
                    bytes,
                    cap: options.max_file_bytes(),
                },
            });
            continue;
        }

        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                outcome.report.files_skipped += 1;
                outcome.skipped.push(SkippedFile {
                    path: relative.to_string(),
                    reason: SkipReason::Unreadable(error.to_string()),
                });
                continue;
            }
        };

        let Some(spec) = crate::extract::registry::get(language) else {
            update = update.removing_file(relative.clone());
            outcome.report.files_unsupported += 1;
            outcome.skipped.push(SkippedFile {
                path: relative.to_string(),
                reason: SkipReason::UnsupportedLanguage(language),
            });
            continue;
        };

        let extracted = crate::extract::extract_with(spec, relative.clone(), &text);
        scope.widen(&extracted);
        touched.push(relative);
        update = absorb_file(extracted, update, &mut outcome);
    }

    if !update.is_empty() {
        let stats = store.apply_update(update)?;
        absorb(&mut outcome.report, &stats);
    }
    outcome.report.generation = store.generation();
    // Scoped to the files that were re-extracted, plus the edges the removal above displaced.
    // Both halves are needed: the first decides the new edges, the second repairs the old ones
    // whose targets no longer exist. Neither is a full re-resolve, so a refresh stays
    // proportional to what changed.
    //
    // The scope is widened to what this batch actually wrote. A default bound is the wrong shape
    // for a scoped pass here: see [`ScopeBounds`].
    let resolution = resolve::resolve_paths(store, &touched, &displaced, scope.resolve_options())?;
    outcome.report.generation = store.generation();
    outcome.report.resolution = Some(resolution);

    // No checkpoint here. A refresh runs per keystroke under `watch`, and the write-ahead log is
    // truncated on a timer regardless; paying a full checkpoint for every save would be the
    // dominant cost of watching. The size is still reported, so a watcher that is somehow not
    // checkpointing shows up as a number rather than as silence.
    measure(&mut outcome.report, store);
    outcome.report.elapsed = started.elapsed();
    Ok(outcome)
}

/// The per-file reads a resolution pass makes, sized from the batch that was just written.
///
/// # Why a default is the wrong shape here
///
/// [`resolve::resolve_paths`] decides the relations of the files it is given by enumerating each
/// file's entities through [`crate::store::Store::entities_in_file`], which is bounded. The default
/// bound is 512 entities, and a file with more than that leaves its tail outside the pass entirely:
/// the relations of those entities are extracted, never placed, and never refused — `Pending`, an
/// edge that answers no question in either direction.
///
/// On `BurntSushi/ripgrep` that is not hypothetical. A refresh of `crates/core/flags/defs.rs`, a
/// file of 1,363 entities, decided 2,121 of the 3,599 relations it wrote and left **348** pending.
/// Nothing later revisits them: a second refresh of the same file left the count at 348, and only a
/// full re-resolve — the pass `build_full` runs and nothing else does — cleared it. `watch` calls
/// `refresh`, so on a watched repository the leak accumulates one large file at a time.
///
/// So the caller measures the shape of what it wrote and widens the bound to match.
///
/// # What this arm is worth now that the resolver pages
///
/// `entities_page` is a page size and the resolver reads **every** page, so widening it is no longer
/// what makes the pass complete — that is what paging does, and it is why the hole above is closed
/// rather than merely narrowed. The arm is kept for one reason, which is that a wider page is a
/// *cheaper* one: with the page set to a batch's largest file, a refresh of `defs.rs` reads it in a
/// single seek instead of three. It is now a performance choice and no longer a correctness one,
/// and the code says so, because a comment that still claimed the tail escaped would be describing
/// a hole that no longer exists.
///
/// # The one above the count
///
/// The resolver records a lookup as truncated when the read returns exactly as many rows as the
/// limit allowed. A bound equal to the row count therefore reports a truncation that did not
/// happen, so the bound is one higher — which also means it cannot truncate at all, which was the
/// property being asked for, and still means a refresh reads each file in one page.
///
/// `incoming_per_entity` is deliberately left alone: it bounds the *re-decision* of edges that
/// already point into a changed file, and an edge left stale there is a different defect from one
/// left undecided.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ScopeBounds {
    /// The most entities any file in the batch declares.
    entities_per_file: usize,
    /// The most relations any one entity in the batch is the source of.
    outgoing_per_source: usize,
}

impl ScopeBounds {
    /// Take one file's extraction into the bound.
    ///
    /// Counting the extraction results rather than the stored rows, so the bound is an upper bound
    /// on what the store now holds: two results sharing an identity become one row, never two.
    fn widen(&mut self, extracted: &ExtractedFile) {
        self.entities_per_file = self.entities_per_file.max(extracted.entities.len());
        let mut per_source: BTreeMap<&EntityId, usize> = BTreeMap::new();
        for relation in &extracted.relations {
            *per_source.entry(&relation.source).or_default() += 1;
        }
        let widest = per_source.values().copied().max().unwrap_or(0);
        self.outgoing_per_source = self.outgoing_per_source.max(widest);
    }

    /// Options whose per-file scope reaches everything this batch wrote.
    fn resolve_options(&self) -> ResolutionOptions {
        let base = ResolutionOptions::default();
        ResolutionOptions {
            entities_page: base.entities_page.max(self.entities_per_file + 1),
            outgoing_per_source: base.outgoing_per_source.max(self.outgoing_per_source + 1),
            ..base
        }
    }
}

/// Read every relation that arrives at an entity declared in `path`.
///
/// Read before the removal that would null their targets. The store's demotion step is what
/// makes the removal safe — a nulled target would otherwise leave a row claiming `resolved`
/// with nothing behind it — and it is also what makes these edges unreachable afterwards:
/// [`Store::incoming`] matches on `target_path`, and a demoted edge has none.
fn collect_incoming(
    store: &Store,
    path: &RepoPath,
    into: &mut Vec<Relation>,
) -> Result<(), StoreError> {
    let mut seen: BTreeSet<RelationKey> = BTreeSet::new();
    for entity in store.entities_in_file(path, DISPLACED_EDGE_SCAN_LIMIT)? {
        for relation in store.incoming(&entity.id, None, DISPLACED_EDGE_SCAN_LIMIT)? {
            if seen.insert(relation.natural_key()) {
                into.push(relation);
            }
        }
    }
    Ok(())
}

/// Read and extract one discovered file, folding it into `update`.
fn ingest(
    file: &DiscoveredFile,
    absolute: &Path,
    update: IndexUpdate,
    outcome: &mut IndexOutcome,
) -> IndexUpdate {
    let Some(spec) = crate::extract::registry::get(file.language) else {
        outcome.report.files_unsupported += 1;
        outcome.skipped.push(SkippedFile {
            path: file.path.to_string(),
            reason: SkipReason::UnsupportedLanguage(file.language),
        });
        return update;
    };

    let text = match std::fs::read_to_string(absolute) {
        Ok(text) => text,
        Err(error) => {
            // A single bad file degrades that file. It never aborts the repository, which is
            // precisely the failure this replaces.
            outcome.report.files_skipped += 1;
            outcome.skipped.push(SkippedFile {
                path: file.path.to_string(),
                reason: SkipReason::Unreadable(error.to_string()),
            });
            return update;
        }
    };

    let extracted = crate::extract::extract_with(spec, file.path.clone(), &text);
    absorb_file(extracted, update, outcome)
}

/// Fold one extraction result into the update, replacing whatever that path contributed before.
///
/// The explicit removal is what makes a rename or a shrinking file correct: an upsert alone
/// would leave rows for symbols that no longer exist.
fn absorb_file(
    extracted: ExtractedFile,
    mut update: IndexUpdate,
    outcome: &mut IndexOutcome,
) -> IndexUpdate {
    outcome.report.files_indexed += 1;
    if !extracted.is_clean() {
        outcome.report.files_degraded += 1;
    }
    outcome.report.tests_found += extracted
        .entities
        .iter()
        .filter(|entity| entity.is_test)
        .count() as u64;

    update = update.removing_file(extracted.path.clone());
    for entity in extracted.entities {
        update = update.with_entity(entity);
    }
    for relation in extracted.relations {
        // Every state is counted. A summary that reported only the three *uncertain* states looked
        // complete but did not add up: on a real build of `rust-lang/regex` it summed to 27,874
        // against a total of 37,868, and nothing in the output said the remaining 9,994 were
        // simply not being reported. A reader could not tell work remaining from work done.
        match &relation.resolution {
            ResolutionState::Pending { .. } => outcome.report.relations_pending += 1,
            ResolutionState::Unresolved { .. } => outcome.report.relations_unresolved += 1,
            ResolutionState::Ambiguous { .. } => outcome.report.relations_ambiguous += 1,
            ResolutionState::Resolved { .. } => outcome.report.relations_resolved += 1,
            ResolutionState::Inferred { .. } => outcome.report.relations_inferred += 1,
        }
        update = update.with_relation(relation);
    }
    update
}

/// Carry the walk's own refusals into the report.
///
/// Discovery *counts* what it refused and, for the cases worth naming, says which file. This
/// copies both across, so a single report answers "how much did you index" and "what did you
/// leave out, and why" without the caller having to run a second walk to find out.
///
/// `unsupported_extension` is deliberately **not** folded into `files_skipped`. A language with
/// no extraction rules is a different fact from a file that could not be read, and the indexer
/// reports them separately for exactly the reason fourteen languages once shipped as "supported"
/// while extracting nothing.
fn absorb_discovery(outcome: &mut IndexOutcome, stats: &DiscoveryStats, issues: &[WalkIssue]) {
    outcome.report.files_unsupported += stats.unsupported_extension as u64;
    outcome.report.files_skipped +=
        (stats.not_utf8 + stats.too_large + stats.unreadable + stats.irregular_skipped) as u64;

    for issue in issues {
        let reason = match &issue.reason {
            WalkIssueReason::UnsupportedExtension { extension } => {
                SkipReason::UnsupportedExtension(extension.clone())
            }
            WalkIssueReason::TooLarge { bytes, cap } => SkipReason::TooLarge {
                bytes: *bytes,
                cap: *cap,
            },
            WalkIssueReason::NotUtf8 { offset } => SkipReason::NotUtf8 { offset: *offset },
            WalkIssueReason::Unreadable { detail } => SkipReason::Unreadable(detail.clone()),
            WalkIssueReason::OutsideRepository => SkipReason::OutsideRoot,
            WalkIssueReason::NonUtf8Path => {
                SkipReason::Unreadable("the path itself is not valid UTF-8".to_owned())
            }
            WalkIssueReason::SymlinkEscapes { target } => {
                SkipReason::Unreadable(format!("symlink resolves to {target}, outside the root"))
            }
            WalkIssueReason::UnresolvableSymlink { .. } => {
                SkipReason::Unreadable("symlink target does not exist".to_owned())
            }
            WalkIssueReason::Duplicate => SkipReason::Unreadable(
                "a case-insensitive twin of this file was already yielded".to_owned(),
            ),
        };
        outcome.skipped.push(SkippedFile {
            path: issue.path.clone(),
            reason,
        });
    }
}

/// Copy the store's own measured counts into the report.///
/// The counts come from the store because the store is what actually did the work. Reporting a
/// number the caller inferred rather than one the writer measured is how statistics become
/// fiction.
fn absorb(report: &mut IndexReport, stats: &UpdateStats) {
    report.entities_written = stats.entities_upserted;
    report.relations_written = stats.relations_upserted;
    report.entities_removed = stats.entities_removed;
}

#[cfg(test)]
mod tests;
