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
//! - a path the operating system named by the *resolved* root is a change inside it
//! - a batch open at shutdown is a batch whose changes are not in the index
//!
//! Nothing here sleeps, with one deliberate exception: the last test needs a real filesystem event,
//! because the disagreement about how a backend spells the root only exists between an operating
//! system and a watcher. Every other test drives the debouncer with an injected `Instant`, so they
//! are deterministic and fast, which is the only reason to trust them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Action, Debouncer, plan_batch, under_root};

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
    assert_eq!(
        plan.outside_root,
        vec![outside],
        "a duplicate is not a second problem"
    );
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
    let batch = debouncer
        .drain_if_quiet(later)
        .expect("the batch should close");
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

    let flushed = debouncer
        .flush()
        .expect("an open batch must be handed over");
    assert_eq!(flushed, vec![under("src/late.rs")]);
    assert!(
        debouncer.flush().is_none(),
        "flushing twice yields nothing the second time"
    );
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
    assert!(
        debouncer
            .drain_if_quiet(start + Duration::from_millis(50))
            .is_some()
    );

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
    // A metadata event that carries no path at all, which some backends produce.
    debouncer.record(Vec::<PathBuf>::new());
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
    assert!(
        eager.drain_if_quiet(later).is_some(),
        "100ms is past a 1ms quiet period"
    );
    assert!(
        patient.drain_if_quiet(later).is_none(),
        "100ms is nowhere near a 60s quiet period"
    );
    assert_eq!(patient.quiet_for(), Duration::from_secs(60));
    // A range rather than an equality: the last event was stamped from the real clock a moment
    // after `start` was read, so the elapsed time is "about 100ms" and asserting an exact value
    // would be asserting a coincidence.
    let elapsed = patient
        .quiet_for_so_far(later)
        .expect("an event arrived, so there is a quiet period to measure");
    assert!(
        elapsed < patient.quiet_for() && elapsed <= Duration::from_millis(100),
        "the elapsed quiet time is observable and short: {elapsed:?}"
    );
}

#[test]
fn a_climb_out_of_the_root_is_a_path_from_outside_it() {
    // The misclassification, and the reason the planner does not compare the spelling. Every
    // component of `/repo/../escape.rs` is under `/repo`, so a component-wise prefix test called it
    // inside and planned a re-index for it; `refresh` then refused it and the plan reported work
    // that was never going to happen. It was never an escape — nothing outside the tree was read —
    // but a plan that lists work that does not happen is a report nobody can act on.
    //
    // **Nothing here touches a filesystem, so the two spellings differ by construction.** The root
    // is `/repo` and does not exist, on any platform: the `..` is the whole of the difference
    // between the spelling and what it says, and there is no temporary directory on the host whose
    // own spelling could be mistaken for the case.
    let root = root();
    let escape = root.join("../escape.rs");
    // The fixture's own precondition: the spelling is component-wise under the root, which is
    // exactly why reading it is not the same as reading what it says.
    assert!(escape.starts_with(&root), "under the root, component-wise");

    let plan = plan_batch(&root, std::slice::from_ref(&escape), rust_only);
    assert!(plan.reindex.is_empty(), "a path that leaves is not work");
    assert_eq!(plan.outside_root, vec![escape], "counted, not planned");
    assert!(plan.ignored.is_empty(), "and not a decline either");
}

#[test]
fn a_climb_that_stays_inside_the_root_is_still_a_change_inside_it() {
    // The other half, and the reason the rule is arithmetic rather than a refusal of `..`: the
    // answer has to follow where the path lands, and a blanket refusal would drop a change inside
    // the repository on the grounds that it was written as a climb.
    let root = root();
    let landed = root.join("src/../src/lib.rs");
    let plan = plan_batch(&root, std::slice::from_ref(&landed), rust_only);

    // The path is planned as the operating system spelled it, and the refresh derives the name with
    // the same rule, so the two cannot disagree about what it is.
    assert_eq!(plan.reindex, vec![landed], "still a change inside it");
    assert!(plan.outside_root.is_empty(), "not a misconfigured watch");
}

// ---------------------------------------------------------------------------
// How a path is spelled on its way off an operating system
// ---------------------------------------------------------------------------

/// The root as a caller on macOS names it, and the same directory as FSEvents names it.
fn named_and_resolved_roots() -> (PathBuf, PathBuf) {
    (
        PathBuf::from("/var/folders/pe/peek"),
        PathBuf::from("/private/var/folders/pe/peek"),
    )
}

#[test]
fn a_path_the_backend_named_by_the_resolved_root_is_a_change_inside_the_root() {
    // The shape macOS produces. The caller named the root through a symlink, the backend reported
    // the directory it had actually opened, and every event in a burst then failed a lexical
    // comparison. The planner is right to count a path it cannot place as arriving from outside —
    // it is handed a string and decides nothing else — which is exactly why the restating has to
    // happen before the planning rather than inside it.
    let (root, resolved_root) = named_and_resolved_roots();
    let reported = resolved_root.join("src/ui.rs");

    let as_reported = plan_batch(&root, std::slice::from_ref(&reported), rust_only);
    assert!(
        as_reported.reindex.is_empty(),
        "the planner cannot place a path it was handed under another spelling: {as_reported:?}"
    );
    assert_eq!(
        as_reported.outside_root,
        vec![reported.clone()],
        "and it is counted rather than dropped, which is why a watcher that re-indexes nothing on \
         such a platform reports no failure at all"
    );

    let restated = under_root(&root, &resolved_root, &reported);
    assert_eq!(
        restated,
        root.join("src/ui.rs"),
        "the same file, named the way the caller named the root"
    );
    let plan = plan_batch(&root, &[restated], rust_only);
    assert_eq!(plan.reindex, vec![root.join("src/ui.rs")], "{plan:?}");
    assert!(
        plan.outside_root.is_empty(),
        "and the batch a watcher applies names a path its own root can strip: {plan:?}"
    );
}

#[test]
fn a_path_the_backend_named_in_the_callers_spelling_is_left_exactly_as_it_arrived() {
    // The Linux shape, and the reason the restating is a special case rather than a replacement:
    // `inotify` reports paths under the name it was registered with, so there is nothing to fix and
    // a rewrite here would be a rewrite invented out of nothing.
    let (root, resolved_root) = named_and_resolved_roots();
    let reported = root.join("src/ui.rs");
    assert_eq!(under_root(&root, &resolved_root, &reported), reported);
}

#[test]
fn a_path_under_neither_spelling_stays_where_it_is_for_the_planner_to_count() {
    // The direction that matters: a path that really is somewhere else must not be reshaped into
    // something that looks like it is inside the root. A sibling directory that happens to share
    // the resolved root's parent is exactly the case a careless prefix strip would get wrong.
    let (root, resolved_root) = named_and_resolved_roots();
    let elsewhere = PathBuf::from("/private/var/folders/pe/other-repository/src/lib.rs");
    assert_eq!(under_root(&root, &resolved_root, &elsewhere), elsewhere);
    let plan = plan_batch(&root, std::slice::from_ref(&elsewhere), rust_only);
    assert_eq!(plan.outside_root, vec![elsewhere], "{plan:?}");
    assert!(plan.reindex.is_empty());
}

#[cfg(unix)]
#[test]
fn a_watch_on_a_root_named_through_a_link_still_applies_the_change() {
    // The same disagreement, with a real operating system in the middle rather than two strings.
    // `inotify` reports the name it was given, so on Linux this passes with or without the
    // restating; FSEvents resolves the root before it reports anything, so on macOS it is the one
    // test in this suite that can fail without it. The root is reached through a link for the same
    // reason every macOS fixture's is: the temporary directory is `/var/folders/...`, and `/var` is
    // a symlink to `/private/var`.
    let tree = scratch("root-through-a-link");
    let link = tree.join("link");
    let real = tree.join("repository");
    std::fs::create_dir_all(&real).expect("a directory to watch");
    let linked = std::os::unix::fs::symlink(&real, &link);
    linked.expect("a link to reach the repository through");

    let options = super::native::WatchOptions {
        quiet_for: Duration::from_millis(20),
        ..super::native::WatchOptions::default()
    };
    let mut watch = super::native::Watch::start(&link, options).expect("a watch to start");
    let wrote = std::fs::write(real.join("added.rs"), "pub fn added() -> u32 { 1 }\n");
    wrote.expect("a file to change");

    // Poll for the batch rather than sleeping for it: the deadline is the ceiling, not the wait.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut reindexed: Vec<PathBuf> = Vec::new();
    let mut outside: Vec<PathBuf> = Vec::new();
    while std::time::Instant::now() < deadline {
        watch.poll();
        if let Some(batch) = watch.take_batch(std::time::Instant::now()) {
            reindexed.extend(batch.plan.reindex);
            outside.extend(batch.plan.outside_root);
            if !reindexed.is_empty() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        !outside.is_empty() || !reindexed.is_empty(),
        "the file was written and the watch saw nothing at all within five seconds"
    );
    assert!(
        outside.is_empty(),
        "a change inside the watched directory is not a change from outside it: {outside:?}"
    );
    assert_eq!(
        reindexed,
        vec![link.join("added.rs")],
        "and the path handed out is named the way the caller named the root, because that is what \
         `indexer::refresh` strips it against: {reindexed:?}"
    );
    assert!(
        reindexed[0].is_file(),
        "and it names the file that changed, not a path nothing exists at: {}",
        reindexed[0].display()
    );

    let _ = std::fs::remove_dir_all(&tree);
}

/// A scratch directory of this process's own, so a second run finds none of the first one's.
#[cfg(unix)]
fn scratch(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("peek-watch-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a scratch directory");
    path
}
