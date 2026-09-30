//! `peek status`: the index's own numbers.
//!
//! # Why this refuses rather than reporting an empty index
//!
//! [`peek_core::store::Store::open`] **creates** a store when the file is absent, so a `status`
//! that simply opened one would answer "generation 0, 0 entities" about a repository that was
//! never indexed — and a caller branching on a zero exit would read that as a measurement. The
//! existence check happens first, and the answer is a refusal carrying the path the index *would*
//! have been at.
//!
//! # What this deliberately does not do
//!
//! No filesystem walk and no integrity check. A walk makes a status report cost the size of the
//! repository, and `integrity_check` reads every page, so both belong to `peek doctor`, which is
//! the command whose job is to be slow. What this leaves out is named below, because a status
//! report that does not say what it did not measure is exactly the confident wrong answer this
//! project exists to remove.

use crate::answer::{Answer, Counts, StatusAnswer};
use crate::commands::{Outcome, need};
use crate::exit::{Failure, Refusal, kind};
use crate::paths::{self, Location};

/// Report the index's generation, counts, location and durability.
pub fn run(location: Option<&Location>, command: &'static str) -> Result<Outcome, Failure> {
    let location = need(location, command)?;
    if !location.index_existed {
        return Err(Failure::refused(
            command,
            Refusal::new(
                kind::NO_INDEX,
                format!(
                    "there is no index for this repository: {} does not exist. `peek index` \
                     builds it. This command refuses rather than opening the store, because \
                     opening a missing index creates one and \"generation 0, nothing indexed\" \
                     would then be indistinguishable from a real measurement",
                    location.index_text()
                ),
            ),
        ));
    }

    let store = paths::open_store(location, command)?;
    let stats = store.stats().map_err(|error| {
        Failure::failed(
            command,
            Refusal::new(
                kind::ENGINE,
                format!("the index's own counts could not be read: {error}"),
            ),
        )
    })?;
    let counts = Counts::from_stats(&stats);
    let answer = StatusAnswer {
        counts,
        generation: store.generation(),
        // Read back from the connection rather than assumed from a constant: `synchronous` is a
        // per-connection setting, so a store opened without it has silently lost its guarantee and
        // the file itself would not say so.
        durability: store.durability().as_str().to_owned(),
        states_partition: counts.states_partition(),
        did_not: vec![
            "this did not walk the repository: it reports only what the store holds. `peek doctor` \
             walks the tree and reports the files it could not index, and `peek index` names every \
             one of them"
                .to_owned(),
            "this did not run SQLite's integrity check, which reads every page and would make a \
             status report cost the size of the index. `peek doctor` runs it"
                .to_owned(),
            "this did not compare the index against the working tree, so nothing here says which \
             files have changed since the last commit. `peek index` re-extracts every discovered \
             file and reports the ones that are no longer on disk"
                .to_owned(),
            "this did not read a byte of source, so it says nothing about whether the code still \
             looks the way this index says it does"
                .to_owned(),
            // Measured, not assumed: a checkpoint a reader refuses still copies the frames it may
            // into the database file, so the two sizes this prints can count the same page twice.
            // `peek doctor`'s wal check measures it across a refused and a completed checkpoint.
            "this did not compare the two file sizes against each other. They are not a partition \
             of the index: the database file holds the pages a checkpoint has copied and the log \
             holds every frame still in it, copied or not, so dividing one by the other measures \
             how much has been copied rather than what the index costs"
                .to_owned(),
        ],
    };

    // A store whose five resolution states do not account for its relations is inconsistent, and a
    // caller cannot check any of the numbers above. That is a fact to report, not a failure of the
    // command, so the status stays `ok` and the flag is in the answer.
    Ok(Outcome::ok(Answer::Status(answer)))
}
