//! Incremental correctness: an incremental rebuild must agree with a full one.
//!
//! # Why this is its own dimension and not a timing claim
//!
//! An earlier A/B measurement on this project found 348 relations stranded as
//! `pending` after an incremental refresh of one file — a number no test could
//! see, because every test read the store after a *full* build. So this gate does
//! the one thing that catches that class of defect:
//!
//! * index a copy of the fixture, change something, refresh;
//! * index a second, byte-identical copy of the changed tree **from scratch**;
//! * compare the two indexes row for row.
//!
//! The two trees are byte-identical and sit at the same depth, so every
//! difference between the indexes is a difference in how the work was done. The
//! comparison is over the whole semantic state, not over a sample, and the
//! undecided count is asserted separately because "the rows agree" and "nothing is
//! outstanding" are different claims.
//!
//! Three operations are exercised, because each fails differently: an **edit**
//! re-extracts a file and can strand edges, a **delete** has to demote the edges
//! that pointed into the file, and a **rename** changes the module name every
//! qualified identity in the file is built from.
//!
//! Each copy is at `<scratch>/gate-fixture`, and the scratch is different for each
//! side. The increment module layout is named after the directory above `src`, so
//! two copies at different depths or under different names would declare
//! different packages and could never be compared at all.

use peek_core::indexer::IndexReport;
use peek_core::model::Language;
use peek_core::store::Store;

use super::measure::{Scratch, Snapshot, build, refresh};
use super::score::Fraction;

/// One operation's result.
#[derive(Debug, Clone)]
pub struct Operation {
    pub name: &'static str,
    /// Rows the two indexes were compared over.
    pub compared: u64,
    /// Rows that were not identical.
    pub differences: Vec<String>,
    /// Relations the index still holds awaiting a decision.
    pub undecided: u64,
    /// Edges in the index pointing at nothing.
    pub orphans: u64,
}

impl Operation {
    pub fn agreed(&self) -> bool {
        self.differences.is_empty() && self.undecided == 0 && self.orphans == 0
    }

    /// One line naming what disagreed, so a failure says where to look.
    pub fn describe(&self) -> String {
        if self.agreed() {
            return format!(
                "{}: {} rows identical, nothing undecided, no dangling edges",
                self.name, self.compared
            );
        }
        format!(
            "{}: {} of {} rows differ, {} undecided, {} dangling; first differences: {}",
            self.name,
            self.differences.len(),
            self.compared,
            self.undecided,
            self.orphans,
            self.differences
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ")
        )
    }
}

/// The whole dimension.
#[derive(Debug, Clone)]
pub struct Incremental {
    pub operations: Vec<Operation>,
    pub agreed: u64,
    pub compared: u64,
}

impl Incremental {
    pub fn fraction(&self) -> Fraction {
        Fraction::new(self.agreed, self.compared)
    }

    pub fn is_clean(&self) -> bool {
        self.operations.iter().all(Operation::agreed)
    }
}

/// The line appended to the fixture's `src/report.rs` before the first refresh.
///
/// A new top-level function, so the operation is not merely "the bytes moved": a
/// symbol appears that no earlier pass could have seen, and every relation out of
/// that file has to be re-decided against the new contents.
const APPENDED: &str = "\
/// Appended by the incremental check, which needs a symbol the earlier passes
/// cannot have seen.
pub fn appended_after_edit() -> u8 {
    7
}
";

/// Run all three operations against a copy of `fixture`.
pub fn run(fixture: &std::path::Path, language: Language) -> Incremental {
    let live_scratch = Scratch::new(&format!("incremental-{}", language.as_str()));
    let live = live_scratch.crate_copy(fixture);
    let (mut store, _) = build(&live);
    let mut operations = Vec::new();

    // -- edit ---------------------------------------------------------------
    let edited = live.join("src/report.rs");
    let mut text = std::fs::read_to_string(&edited).expect("read the file being edited");
    text.push('\n');
    text.push_str(APPENDED);
    std::fs::write(&edited, &text).expect("write the edited file");
    let report = refresh(&mut store, &live, std::slice::from_ref(&edited));
    operations.push(agree("edit src/report.rs", &live, &store, &report));

    // -- delete -------------------------------------------------------------
    let removed = live.join("src/model.rs");
    std::fs::remove_file(&removed).expect("delete the file under test");
    let report = refresh(&mut store, &live, std::slice::from_ref(&removed));
    operations.push(agree("delete src/model.rs", &live, &store, &report));

    // -- rename -------------------------------------------------------------
    let before = live.join("src/service.rs");
    let after = live.join("src/worker.rs");
    std::fs::rename(&before, &after).expect("rename the file under test");
    let report = refresh(&mut store, &live, &[before.clone(), after.clone()]);
    operations.push(agree(
        "rename src/service.rs to src/worker.rs",
        &live,
        &store,
        &report,
    ));

    let agreed: u64 = operations
        .iter()
        .map(|operation| operation.compared - operation.differences.len() as u64)
        .sum();
    let compared: u64 = operations.iter().map(|operation| operation.compared).sum();
    Incremental {
        operations,
        agreed,
        compared,
    }
}

/// Index a fresh copy of `live` from scratch and compare it with `store`.
fn agree(
    name: &'static str,
    live: &std::path::Path,
    store: &Store,
    report: &IndexReport,
) -> Operation {
    let reference_scratch = Scratch::new("incremental-reference");
    let reference = reference_scratch.crate_copy(live);
    let (reference_store, _) = build(&reference);

    let left = Snapshot::of(store);
    let right = Snapshot::of(&reference_store);
    let differences = left.differences(&right);
    let stats = store.stats().expect("stats after the refresh");
    Operation {
        name,
        compared: left.rows() + right.rows(),
        differences,
        // The store's own figure, raised to the refresh's own figure if the
        // refresh claims more outstanding than the store holds. Both are read
        // rather than one being inferred from the other, and the larger wins:
        // "this run decided everything it wrote" and "this index has nothing
        // outstanding" are different claims and only one of them is a claim the
        // reader can rely on.
        undecided: stats.pending_relations.max(report.relations_undecided),
        orphans: stats.orphan_relations,
    }
}
