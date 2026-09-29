//! `peek rm`: forget a file or a subtree.
//!
//! # It touches the index and nothing else
//!
//! The command is named `rm` and it removes **rows**. No file is unlinked, no directory is deleted,
//! and the working tree is byte-for-byte what it was. Stating that in the answer is not
//! politeness: a command called `rm` that appears to delete source is a command that will be run
//! with the wrong expectations, and the cost of finding out is somebody's work.
//!
//! # The edges that pointed in
//!
//! The store demotes them. A relation whose target left the index becomes an honest
//! `unresolved (no_candidate)` rather than a row claiming `resolved` with nothing behind it, which
//! is contract D4 and the reason a removal is safe to answer questions afterwards. The resolver is
//! not re-run: the target is genuinely gone, so `unresolved` is the correct terminal state, and
//! re-deciding would produce nothing. If the code comes back, `peek index` re-extracts the files
//! that held the edges and decides them again.
//!
//! # Scope, decided by what is on disk
//!
//! A path that names a directory removes the subtree; anything else removes exactly that file. The
//! scope is reported, because "I removed `src`" and "I removed `src/main.rs`" are different claims
//! and a caller branching on the result needs to know which it got.

use peek_core::store::IndexUpdate;

use crate::answer::{Answer, RemoveAnswer};
use crate::commands::{Outcome, need};
use crate::exit::{Failure, Refusal, kind};
use crate::paths::{self, Location};

/// Remove a path from the index.
pub fn run(
    location: Option<&Location>,
    command: &'static str,
    given: &str,
) -> Result<Outcome, Failure> {
    let location = need(location, command)?;
    // Before opening, because opening creates. `rm` on a repository with no index has nothing to
    // remove and must not leave one behind as a side effect of being asked.
    if !location.index_existed {
        return Err(Failure::refused(
            command,
            Refusal::new(
                kind::NO_INDEX,
                format!(
                    "there is no index for this repository: {} does not exist, so there is \
                     nothing to remove from it. This command refuses rather than opening the \
                     store, because opening a missing index creates one",
                    location.index_text()
                ),
            ),
        ));
    }
    // The containment test, and the case-sensitivity it depends on, are in `paths::relative_to`.
    let relative = paths::relative_to(&location.root, given, command)?;

    let mut store = paths::open_store(location, command)?;
    // Measured before the write, because afterwards the rows are gone and reporting a number
    // nobody could check would be reporting fiction.
    let indexed = paths::indexed_path_set(&store, command)?;
    let covered = indexed
        .iter()
        .filter(|path| path.is_within(&relative.path))
        .count() as u64;

    let scope = if relative.is_directory {
        "subtree"
    } else {
        "file"
    };
    let update = if relative.is_directory {
        IndexUpdate::empty().removing_subtree(relative.path.clone())
    } else {
        IndexUpdate::empty().removing_file(relative.path.clone())
    };
    let stats = store.apply_update(update).map_err(|error| {
        Failure::failed(
            command,
            Refusal::new(
                kind::ENGINE,
                format!(
                    "{} could not be removed from the index: {error}",
                    relative.path
                ),
            ),
        )
    })?;

    let mut notes = vec![
        "edges that pointed into the removed scope are now recorded as `unresolved \
         (no_candidate)`: the target is no longer in the index, which is a fact about the index \
         and not about the code. `peek index` decides them again if the code returns"
            .to_owned(),
    ];
    if covered == 0 {
        notes.push(
            "the index held no rows at that path, so the zero counts above is a measurement and \
             not an unperformed check"
                .to_owned(),
        );
    }
    let mut did_not = vec![
        "this did not touch the filesystem: no file was unlinked and no directory was deleted, and \
         every file is on disk exactly as it was"
            .to_owned(),
        "this did not run the resolver, so the relations that pointed into the removed scope stay \
         undecided until the files that hold them are re-indexed"
            .to_owned(),
    ];
    if relative.is_directory {
        did_not.push(
            "this removed the whole subtree, including the file at the path itself. `peek rm \
             <file>` is the form that removes exactly one file"
                .to_owned(),
        );
    }

    let answer = RemoveAnswer {
        path: relative.path.to_string(),
        scope: scope.to_owned(),
        indexed_paths_removed: covered,
        entities_removed: stats.entities_removed,
        relations_removed: stats.relations_removed,
        generation: stats.generation,
        notes,
        did_not,
    };
    Ok(Outcome::ok(Answer::Remove(answer)))
}
