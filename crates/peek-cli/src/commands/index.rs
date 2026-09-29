//! `peek index`: build, or refresh what is there.
//!
//! # The three modes, and why there is no fourth
//!
//! | mode | when | what it does |
//! |---|---|---|
//! | `build` | the store has never committed a generation | `indexer::build_full` |
//! | `refresh` | a generation exists | `indexer::refresh` over every discovered path, plus every indexed path that is gone from disk |
//! | `rebuild` | `--full` | clear, then build |
//!
//! **Cold is decided by the generation, not by an entity count.** `Store::open` creates a store
//! at generation 0 and every commit advances the counter, so "generation 0" is exactly "nothing
//! has ever been committed through the write path" — a documented property of the store rather
//! than a heuristic. Counting entities to decide it would be a table scan on every invocation, for
//! no extra certainty.
//!
//! # How a deleted file is noticed
//!
//! A file that was deleted is not in the discovery walk, so a refresh of what the walk yields
//! would leave it indexed forever — a stale index that still answers questions about code that is
//! gone. `paths::indexed_paths` reads the index's own path list and the difference is what gets
//! handed to the refresh alongside the discovered files, so a deletion is a removal like any
//! other. That function is a stopgap over `Store::conn`, and it says so.
//!
//! # What `--full` actually does
//!
//! It issues one transaction removing every top-level entry the index holds, then builds. Not a
//! file deletion: the index is a single SQLite file, and unlinking it would leave a crash between
//! the unlink and the rebuild with no index at all. A cleared index is a valid, openable,
//! diagnosable state — `doctor` reports it as empty, with the action `run peek index` — which a
//! missing file is not.
//!
//! The entries cleared are named in the answer, and only the ones that held indexed data, so
//! `cleared` is a claim about rows rather than about directory names.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use peek_core::discover::{DiscoveryOptions, FileDiscovery};
use peek_core::indexer;
use peek_core::model::RepoPath;
use peek_core::store::IndexUpdate;

use crate::answer::{Answer, IndexAnswer, ResolutionAnswer, SkippedFileAnswer, millis};
use crate::commands::{Outcome, need};
use crate::exit::{Failure, Refusal, kind};
use crate::paths::{self, Location};
use crate::progress::Progress;

/// Build or refresh the index for a repository.
pub fn run(
    location: Option<&Location>,
    command: &'static str,
    full: bool,
    progress: &mut dyn Progress,
) -> Result<Outcome, Failure> {
    let location = need(location, command)?;
    let mut store = paths::open_store(location, command)?;
    let mut notes: Vec<String> = Vec::new();
    let mut did_not: Vec<String> = Vec::new();
    let mut cleared: Vec<String> = Vec::new();
    let mut clear_removed: u64 = 0;
    let mut stale_paths_removed: u64 = 0;

    if full {
        let cleared_paths = clear_index(&mut store, &location.root, command)?;
        clear_removed = cleared_paths.entities_removed;
        cleared = cleared_paths.names;
        if cleared.is_empty() {
            notes.push(
                "the index held nothing, so `--full` had nothing to clear and the build below is \
                 the same as a cold one"
                    .to_owned(),
            );
        } else {
            progress.note(format!(
                "cleared {} top-level entr(ies) holding {} entity row(s)",
                cleared.len(),
                clear_removed
            ));
            did_not.push(
                "the index was cleared before the build, and the two are separate transactions: a \
                 crash between them leaves an empty index at the generation printed above, which \
                 `peek doctor` reports as empty with the action `run peek index`"
                    .to_owned(),
            );
        }
    }

    // Cold is "nothing has ever been committed", which the generation answers exactly.
    let cold = store.generation() == 0;
    let options = DiscoveryOptions::default();

    let (mode, outcome) = if cold {
        progress.note("no generation has been committed here; building the index".to_owned());
        let outcome =
            indexer::build_full(&mut store, &location.root, options).map_err(|error| {
                Failure::failed(
                    command,
                    Refusal::new(
                        kind::ENGINE,
                        format!("the index could not be built: {error}"),
                    ),
                )
            })?;
        ("build", outcome)
    } else {
        let (batch, stale) = refresh_batch(&location.root, &store, command)?;
        stale_paths_removed = stale;
        progress.note(format!(
            "refreshing {} path(s), {} of which are no longer on disk",
            batch.len(),
            stale_paths_removed
        ));
        let outcome =
            indexer::refresh(&mut store, &location.root, &batch, &options).map_err(|error| {
                Failure::failed(
                    command,
                    Refusal::new(
                        kind::ENGINE,
                        format!("the index could not be refreshed: {error}"),
                    ),
                )
            })?;
        ("refresh", outcome)
    };

    let mode = if full { "rebuild" } else { mode };
    let report = outcome.report();
    if !outcome.skipped.is_empty() {
        notes.push(format!(
            "{} file(s) were considered and not indexed; each is named below with the reason",
            outcome.skipped.len()
        ));
    }
    if report.files_unsupported > 0 {
        did_not.push(format!(
            "{} file(s) have no registered extraction rules. That is not the same as having no \
             symbols, and the count is the honest measure of it",
            report.files_unsupported
        ));
    }
    did_not.push(
        "this did not compare file contents: a warm run re-extracts every file discovery yields, so \
         it cannot say which of them were unchanged"
            .to_owned(),
    );
    did_not.push(
        "this noticed a deleted file by comparing the index's own path list against the walk. It \
         did not diff anything, so a branch switch is handled by re-extracting everything, which \
         is correct but is not a reconciliation"
            .to_owned(),
    );
    did_not.push(
        "this did not run the resolver over the whole index, only over the files it wrote and the \
         edges that pointed into them. A relation elsewhere that is still undecided is untouched"
            .to_owned(),
    );

    let skipped: Vec<SkippedFileAnswer> = outcome
        .skipped
        .iter()
        .map(SkippedFileAnswer::from)
        .collect();
    let mut skipped_by_reason: BTreeMap<String, u64> = BTreeMap::new();
    for entry in &skipped {
        *skipped_by_reason.entry(entry.reason.clone()).or_default() += 1;
    }

    let answer = IndexAnswer {
        mode: mode.to_owned(),
        generation: report.generation,
        files_indexed: report.files_indexed,
        files_removed: report.files_removed,
        files_skipped: report.files_skipped,
        files_unsupported: report.files_unsupported,
        files_degraded: report.files_degraded,
        stale_paths_removed,
        cleared_paths: cleared,
        entities_written: report.entities_written,
        // The clear's removals and the run's own, added: each is a real measurement by the store
        // and reporting only the second would understate a `--full` by the whole index.
        entities_removed: report.entities_removed.saturating_add(clear_removed),
        relations_written: report.relations_written,
        tests_found: report.tests_found,
        skipped,
        skipped_by_reason,
        resolution: report.resolution.as_ref().map(ResolutionAnswer::from),
        resolution_states_partition: crate::answer::report_partitions(report),
        elapsed_ms: millis(report.elapsed),
        notes,
        did_not,
    };
    Ok(Outcome::ok(Answer::Index(answer)))
}

/// What a `--full` clear took out.
struct Cleared {
    /// The top-level entries, as repository-relative paths.
    names: Vec<String>,
    /// Entity rows the store deleted, measured.
    entities_removed: u64,
}

/// Remove every top-level entry the index holds, in one transaction.
fn clear_index(
    store: &mut peek_core::store::Store,
    root: &std::path::Path,
    command: &'static str,
) -> Result<Cleared, Failure> {
    let indexed = paths::indexed_path_set(store, command)?;
    if indexed.is_empty() {
        return Ok(Cleared {
            names: Vec::new(),
            entities_removed: 0,
        });
    }
    let entries = std::fs::read_dir(root).map_err(|error| {
        Failure::failed(
            command,
            Refusal::new(
                kind::ENGINE,
                format!("cannot list {}: {error}", root.display()),
            ),
        )
    })?;
    let mut update = IndexUpdate::empty();
    let mut names: Vec<RepoPath> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = RepoPath::new(name.to_string_lossy().as_ref()) else {
            // A top-level name the model would refuse to construct cannot have rows under it,
            // because a row's path is the name the model would have written. Skipping it is
            // arithmetic, not a silent omission.
            continue;
        };
        if !indexed.iter().any(|path| path.is_within(&name)) {
            continue;
        }
        update = update.removing_subtree(name.clone());
        names.push(name);
    }
    if names.is_empty() {
        return Ok(Cleared {
            names: Vec::new(),
            entities_removed: 0,
        });
    }
    let stats = store.apply_update(update).map_err(|error| {
        Failure::failed(
            command,
            Refusal::new(
                kind::ENGINE,
                format!("the index could not be cleared: {error}"),
            ),
        )
    })?;
    Ok(Cleared {
        names: names.iter().map(RepoPath::to_string).collect(),
        entities_removed: stats.entities_removed,
    })
}

/// The paths to hand to `indexer::refresh`, and how many of them were stale.
///
/// Two sets, unioned: everything the walk yields, and everything the index holds that the walk no
/// longer yields. The second is the half that makes a deletion a removal.
fn refresh_batch(
    root: &std::path::Path,
    store: &peek_core::store::Store,
    command: &'static str,
) -> Result<(Vec<PathBuf>, u64), Failure> {
    let discovery = FileDiscovery::new(root, DiscoveryOptions::default())
        .discover()
        .map_err(|error| {
            Failure::failed(
                command,
                Refusal::new(
                    kind::ENGINE,
                    format!("the repository could not be walked: {error}"),
                ),
            )
        })?;
    let discovered: BTreeSet<RepoPath> = discovery
        .files()
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let indexed = paths::indexed_path_set(store, command)?;
    let stale: Vec<&RepoPath> = indexed
        .iter()
        .filter(|path| !discovered.contains(*path))
        .collect();

    let mut batch: Vec<PathBuf> = discovered
        .iter()
        .map(|path| root.join(path.as_str()))
        .collect();
    for path in &stale {
        batch.push(root.join(path.as_str()));
    }
    Ok((batch, stale.len() as u64))
}
