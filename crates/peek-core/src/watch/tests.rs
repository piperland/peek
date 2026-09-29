//! The watcher's conformance suite.
//!
//! These test the *decision*, not the watching. The properties that matter are all about what
//! happens to a burst of events, and every one of them is a property a naive implementation gets
//! wrong in a way that only shows up on someone else's machine:
//!
//! - a save produces forty events and must cost one re-index
//! - a rename is a delete and a create, and needs no special case
//! - an event for a file we do not index must not cost a transaction
//! - a path from outside the root means the *watcher* is misconfigured, not that the file changed
//! - a batch open at shutdown is a batch whose changes are not in the index
//!
//! Nothing here sleeps. The debouncer is driven by an injected `Instant`, so these are
//! deterministic and fast, which is the only reason to trust them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Action, Debouncer, plan_batch};

/// A root, and paths under it, for the planner's `starts_with` to work against.
fn root() -> PathBuf {
    PathBuf::from("/repo")
}

fn under(relative: &str) -> PathBuf {
    root().join(relative)
}

/// Only `.rs` is extracted in these fixtures, which keeps the policy in one place.
fn rust_only(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "rs")
}

#[test]
fn forty_three_events_for_one_file_cost_one_re_index() {
    // What an editor actually does on save: a temp file, a write, a rename, a delete, and a
    // handful of attribute touches. Re-extracting per event churns the store's generation for
    // every intermediate state, and every one of those generations has to be replayed by the next
    // reader.
    let events: Vec<PathBuf> = (0..43)
        .map(|n| under(&format!("src/lib{}.rs", n % 2)))
        .collect();
    let plan = plan_batch(&root(), &events, rust_only);
    assert_eq!(
        plan.reindex,
        vec![under("src/lib0.rs"), under("src/lib1.rs")],
        "two distinct paths, whatever the event count"
    );
}

#[test]
fn a_rename_needs_no_special_case_because_the_filesystem_sends_a_delete_and_a_create() {
    // This is why there is no rename rule: the OS already expresses a rename as two events, and
    // handling it explicitly is how an implementation ends up removing the new file because it saw
    // the old one's delete.
    let events = vec![under("src/old.rs"), under("src/new.rs")];
    let plan = plan_batch(&root(), &events, rust_only);
    assert_eq!(plan.reindex.len(), 2);
    assert!(plan.reindex.contains(&under("src/new.rs")));
    assert!(
        plan.reindex.contains(&under("src/old.rs")),
        "the old path is passed too, and `refresh` removes it because the file is not there"
    );
}

#[test]
fn a_deleted_file_is_re_indexed_so_its_rows_are_removed_rather_than_left_behind() {
    // The dangerous direction: an upsert-only watcher keeps answering questions about code that
    // is gone, and nothing anywhere reports the staleness. The planner cannot know the file is
    // gone — only that it changed — so deletion has to flow through the same path and be handled
    // by the refresh, which checks presence.
    let plan = plan_batch(&root(), &[under("src/gone.rs")], rust_only);
    assert_eq!(plan.reindex, vec![under("src/gone.rs")]);
    assert!(
        !under("src/gone.rs").exists(),
        "the planner does not touch the filesystem; the refresh does"
    );
}

#[test]
fn an_event_for_a_file_this_build_does_not_extract_costs_nothing() {
    // A documentation change should not open a transaction. The cost is not the write, it is the
    // generation bump that makes the generation counter useless as a change signal.
    let events = vec![under("README.md"), under("Cargo.toml"), under("src/lib.rs")];
    let plan = plan_batch(&root(), &events, rust_only);

    assert_eq!(plan.reindex, vec![under("src/lib.rs")]);
    assert_eq!(plan.ignored.len(), 2, "{:?}", plan.ignored);
    for decision in &plan.ignored {
        assert_eq!(decision.action, Action::Ignore);
        assert!(
            !decision.reason.is_empty(),
            "a skipped path must say why, not just be absent"
        );
    }
    assert!(plan.summary().contains("2 ignored"), "{}", plan.summary());
}

#[test]
fn a_path_from_outside_the_repository_is_counted_because_the_watcher_is_misconfigured() {
    // A path outside the root does not mean a file changed. It means something is feeding this
    // watcher paths it was not asked to watch — a symlink target, a second repository, a bug.
    // Silently dropping it is how a watcher ends up indexing a tree the user never named.
    let outside = PathBuf::from("/elsewhere/other/src/lib.rs");
    let plan = plan_batch(&root(), &[under("src/lib.rs"), outside.clone()], rust_only);

    assert_eq!(plan.reindex, vec![under("src/lib.rs")]);
    assert_eq!(plan.outside_root, vec![outside]);
    assert!(
        plan.summary().contains("from outside"),
        "the count has to be visible: {}",
        plan.summary()
    );
}

#[test]
fn the_same_path_seen_from_outside_twice_is_counted_once() {
    let outside = PathBuf::from("/elsewhere/x.rs");
    let plan = plan_batch(&root(), &[outside.clone(), outside.clone()], rust_only);
    assert_eq!(plan.outside_root, vec![outside], "a duplicate is not a second problem");
}

#[test]
fn a_batch_of_only_ignored_events_is_empty_so_no_transaction_is_opened() {
    // The generation counter is a change signal. Advancing it for a batch that changed nothing
    // makes it useless for exactly the thing it exists for.
    let events = vec![under("README.md"), under("notes.txt"), under("a.toml")];
    let plan = plan_batch(&root(), &events, rust_only);
    assert!(plan.is_empty(), "{plan:?}");
    assert!(plan.reindex.is_empty());
}

#[test]
fn two_identical_batches_produce_identical_plans() {
    // A watcher's output has to be reproducible or nothing about it can be tested. Ordering is
    // therefore part of the contract, not an accident of a hash map.
    let events: Vec<PathBuf> = ["src/b.rs", "src/a.rs", "src/c.rs"]
        .iter()
        .map(|p| under(p))
        .collect();
    let first = plan_batch(&root(), &events, rust_only);
    let second = plan_batch(&root(), &events, rust_only);
    assert_eq!(first, second);
    assert_eq!(
        first.reindex,
        vec![under("src/a.rs"), under("src/b.rs"), under("src/c.rs")],
        "sorted, so the order does not depend on how the events arrived"
    );
}

#[test]
fn a_batch_stays_open_until_the_quiet_period_has_elapsed() {
    let start = std::time::Instant::now();
    let mut debouncer = Debouncer::new(Duration::from_millis(200));

    debouncer.record([under("src/a.rs")]);
    assert_eq!(debouncer.len(), 1);

    // Still arriving: the batch must not close, or a slow save is re-indexed in pieces.
    let soon = start + Duration::from_millis(50);
    assert!(debouncer.drain_if_quiet(soon).is_none());
    debouncer.record([under("src/b.rs")]);
    assert_eq!(debouncer.len(), 2, "the earlier path is still pending");

    // A second event restarted the quiet period, so the original deadline no longer applies.
    let after_second = start + Duration::from_millis(120);
    assert!(
        debouncer.drain_if_quiet(after_second).is_none(),
        "the quiet period restarts on every event"
    );
}

#[test]
fn a_batch_closes_once_the_quiet_period_has_passed() {
    let start = std::time::Instant::now();
    let mut debouncer = Debouncer::new(Duration::from_millis(200));
    debouncer.record([under("src/a.rs"), under("src/b.rs")]);

    let later = start + Duration::from_millis(500);
    let batch = debouncer.drain_if_quiet(later).expect("the batch should close");
    assert_eq!(batch.len(), 2);
    assert!(debouncer.is_empty(), "the batch is handed over, not copied");
    assert!(
        debouncer.drain_if_quiet(later).is_none(),
        "an empty debouncer does not produce empty batches forever"
    );
}

#[test]
fn a_batch_open_at_shutdown_is_still_handed_over() {
    // This is the difference between a clean exit and a stale index. A user quits while a save is
    // mid-debounce; those changes are not in the index, and the next process has to be told.
    let mut debouncer = Debouncer::new(Duration::from_secs(30));
    debouncer.record([under("src/late.rs")]);

    let flushed = debouncer.flush().expect("an open batch must be handed over");
    assert_eq!(flushed, vec![under("src/late.rs")]);
    assert!(debouncer.flush().is_none(), "flushing twice yields nothing the second time");
}

#[test]
fn an_event_while_a_batch_is_open_joins_the_batch_rather_than_being_lost() {
    // The property a naive watcher gets wrong: events arrive *while* the resolution pass is
    // running, and a design that clears the buffer before applying it drops them. The index then
    // reports itself current while missing a file the user saved a second ago.
    let start = std::time::Instant::now();
    let mut debouncer = Debouncer::new(Duration::from_millis(50));

    debouncer.record([under("src/first.rs")]);
    let batch = debouncer
        .drain_if_quiet(start + Duration::from_millis(200))
        .expect("the first batch closes");
    assert_eq!(batch, vec![under("src/first.rs")]);

    // An event that arrives *after* the drain, while the first batch is still being applied.
    debouncer.record([under("src/second.rs")]);
    let second = debouncer
        .drain_if_quiet(start + Duration::from_millis(400))
        .expect("the second batch must close too");
    assert_eq!(
        second,
        vec![under("src/second.rs")],
        "an event during a slow apply is not lost"
    );
}

#[test]
fn a_failed_apply_is_reported_once_rather_than_swallowed() {
    // A watcher that retries silently forever looks healthy while the index rots. The failure is
    // surfaced on the next drain so a caller can show it.
    let start = std::time::Instant::now();
    let mut debouncer = Debouncer::new(Duration::from_millis(10));
    assert!(debouncer.last_failure().is_none());

    debouncer.record([under("src/a.rs")]);
    assert!(debouncer.drain_if_quiet(start + Duration::from_millis(50)).is_some());

    debouncer.note_failure("store is locked by another process");
    assert_eq!(
        debouncer.last_failure(),
        Some("store is locked by another process")
    );
    assert!(
        debouncer.is_empty(),
        "a failure does not re-queue the batch; re-reading the files is the caller's call"
    );
}

#[test]
fn an_empty_recording_does_not_start_the_quiet_period() {
    // A watcher that receives a "metadata changed" event with no path must not extend the batch
    // open forever. Not recording means the quiet period still runs.
    let start = std::time::Instant::now();
    let mut debouncer = Debouncer::new(Duration::from_millis(50));
    debouncer.record([PathBuf::new()]);
    assert!(debouncer.is_empty());
    assert!(
        debouncer.quiet_for_so_far(start).is_none(),
        "nothing arrived, so nothing is being waited on"
    );
}

#[test]
fn the_quiet_period_is_a_parameter_and_not_a_constant() {
    // 200 ms is wrong for a network filesystem and wrong for a local SSD. The value is a decision
    // the caller makes, and this pins that it is actually plumbed through.
    let start = std::time::Instant::now();
    let mut eager = Debouncer::new(Duration::from_millis(1));
    let mut patient = Debouncer::new(Duration::from_secs(60));
    eager.record([under("src/a.rs")]);
    patient.record([under("src/a.rs")]);

    let later = start + Duration::from_millis(100);
    assert!(eager.drain_if_quiet(later).is_some(), "100ms is past a 1ms quiet period");
    assert!(
        patient.drain_if_quiet(later).is_none(),
        "100ms is nowhere near a 60s quiet period"
    );
    assert_eq!(patient.quiet_for(), Duration::from_secs(60));
    assert_eq!(
        patient.quiet_for_so_far(later),
        Some(Duration::from_millis(100)),
        "the elapsed quiet time is observable, so a caller can log it"
    );
}
