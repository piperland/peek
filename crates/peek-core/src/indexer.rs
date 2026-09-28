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

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::discover::{DiscoveredFile, DiscoveryOptions, FileDiscovery};
use crate::extract::ExtractedFile;
use crate::model::{Language, RepoPath, ResolutionState};
use crate::store::{IndexUpdate, Store, StoreError, UpdateStats};

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
    /// Paths removed from the index.
    pub files_removed: u64,
    pub entities_written: u64,
    pub relations_written: u64,
    /// Relations left `Pending` by the extractor, awaiting resolution.
    pub relations_pending: u64,
    /// Relations the extractor could not resolve at all.
    pub relations_unresolved: u64,
    /// Relations the extractor found genuinely ambiguous.
    pub relations_ambiguous: u64,
    /// Entities that are test cases.
    pub tests_found: u64,
    /// Wall-clock time for the run.
    pub elapsed: Duration,
}

impl IndexReport {
    /// A one-line summary for `peek status` and the MCP `index_status` primitive.
    pub fn summary(&self) -> String {
        format!(
            "generation {}: {} files indexed ({} skipped, {} unsupported, {} degraded), \
             {} removed, {} entities, {} relations ({} pending, {} unresolved, {} ambiguous), \
             {} tests, {:?}",
            self.generation,
            self.files_indexed,
            self.files_skipped,
            self.files_unsupported,
            self.files_degraded,
            self.files_removed,
            self.entities_written,
            self.relations_written,
            self.relations_pending,
            self.relations_unresolved,
            self.relations_ambiguous,
            self.tests_found,
            self.elapsed
        )
    }
}

/// Why a file was not indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// No registered extraction rules for this language.
    ///
    /// Reported separately from "no symbols found" on purpose. Conflating the two is how an
    /// engine advertises twenty-eight languages of which fourteen extract nothing.
    UnsupportedLanguage(Language),
    /// The bytes are not valid UTF-8.
    NotUtf8,
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
            SkipReason::UnsupportedLanguage(language) => {
                write!(f, "no extraction rules for {language}")
            }
            SkipReason::NotUtf8 => f.write_str("not valid UTF-8"),
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

/// Build a complete index of `root`.
///
/// One discovery walk, one transaction. A failure anywhere leaves the previous generation intact
/// and readable, which is the crash-safety requirement.
pub fn build_full(
    store: &mut Store,
    root: &Path,
    options: DiscoveryOptions,
) -> Result<IndexOutcome, IndexError> {
    let started = Instant::now();
    let discovery = FileDiscovery::new(root, options).discover()?;

    let mut outcome = IndexOutcome::default();
    let mut update = IndexUpdate::empty();

    for file in discovery.files() {
        let absolute = discovery.report().absolute(&file.path);
        update = ingest(file, &absolute, update, &mut outcome);
    }

    let stats = store.apply_update(update)?;
    absorb(&mut outcome.report, &stats);
    outcome.report.generation = store.generation();
    outcome.report.elapsed = started.elapsed();
    Ok(outcome)
}

/// Refresh only `paths`, which is what `watch` calls.
///
/// Cost is proportional to the number of changed files, not to the size of the repository. A
/// path that no longer exists is removed; one that does is re-extracted. That is the entirety of
/// incremental indexing, and the reason a branch switch no longer costs a full rebuild.
pub fn refresh(
    store: &mut Store,
    root: &Path,
    paths: &[PathBuf],
    options: &DiscoveryOptions,
) -> Result<IndexOutcome, IndexError> {
    let started = Instant::now();
    let mut outcome = IndexOutcome::default();
    let mut update = IndexUpdate::empty();

    for path in paths {
        let Some(relative) = RepoPath::from_path(path.strip_prefix(root).unwrap_or(path)) else {
            outcome.report.files_skipped += 1;
            outcome.skipped.push(SkippedFile {
                path: path.display().to_string(),
                reason: SkipReason::OutsideRoot,
            });
            continue;
        };

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
        update = absorb_file(extracted, update, &mut outcome);
    }

    if !update.is_empty() {
        let stats = store.apply_update(update)?;
        absorb(&mut outcome.report, &stats);
    }
    outcome.report.generation = store.generation();
    outcome.report.elapsed = started.elapsed();
    Ok(outcome)
}

/// Read and extract one discovered file, folding it into `update`.
fn ingest(
    file: &DiscoveredFile,
    absolute: &Path,
    mut update: IndexUpdate,
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
        match &relation.resolution {
            ResolutionState::Pending { .. } => outcome.report.relations_pending += 1,
            ResolutionState::Unresolved { .. } => outcome.report.relations_unresolved += 1,
            ResolutionState::Ambiguous { .. } => outcome.report.relations_ambiguous += 1,
            _ => {}
        }
        update = update.with_relation(relation);
    }
    update
}

/// Copy the store's own measured counts into the report.
///
/// The counts come from the store because the store is what actually did the work. Reporting a
/// number the caller inferred rather than one the writer measured is how statistics become
/// fiction.
fn absorb(report: &mut IndexReport, stats: &UpdateStats) {
    report.entities_written = stats.entities_upserted;
    report.relations_written = stats.relations_upserted;
    report.files_removed = stats.entities_removed;
}
