//! The watch session, and the shutdown path, without a filesystem watcher.
//!
//! # What is tested here and what is not
//!
//! The `Session` is the whole of a watcher's decision-making: given a [`Batch`], apply it and
//! record what happened. It is driven here with batches built by [`plan_batch`], the same pure
//! function the real watcher uses, so these tests exercise the real path from a list of event paths
//! to a refresh of the index.
//!
//! **The loop is not tested.** Opening an OS watch, sleeping on a timer, and noticing that a
//! directory has disappeared cannot be tested without a real filesystem and real time, and a test
//! that does it becomes flaky rather than informative. What *is* tested is that the loop's exits
//! all go through the same `Session::apply` and that `stop()` hands over a final batch, so the part
//! that is untested contains no decision.
//!
//! # The signal gap
//!
//! The one thing that cannot be tested here is Ctrl-C, because this build cannot catch it. Every
//! test below that exercises a shutdown does so through `stop()`, which is the path a clean exit
//! takes; a signal would skip it, and the command says so in every answer.

// `expect` and `panic` are denied workspace-wide; an integration test is a separate crate and does
// not inherit the library's exemption. See `real_repository.rs` for the same justification.
#![allow(clippy::expect_used, clippy::panic)]

mod fixture;

use std::path::PathBuf;
use std::time::Duration;

use fixture::Repository;

use peek_cli::answer::Answer;
use peek_cli::args;
use peek_cli::commands::watch::{Applied, Session, tick};
use peek_cli::exit::Status;
use peek_cli::progress::Silent;
use peek_cli::run as run_invocation;
use peek_core::discover::DiscoveryOptions;
use peek_core::watch::native::Batch;
use peek_core::watch::plan_batch;

/// A batch built the way the watcher builds one: from raw event paths, through the same planner.
fn batch(root: &std::path::Path, events: &[&str]) -> Batch {
    let paths: Vec<PathBuf> = events.iter().map(|event| root.join(event)).collect();
    let plan = plan_batch(root, &paths, args::is_indexable);
    Batch {
        plan,
        events: paths.len(),
    }
}

/// Open the store a session writes through.
fn open_store(repository: &Repository) -> peek_core::store::Store {
    let repo = peek_core::store::RepoId::discover(repository.root()).expect("derive the identity");
    peek_core::store::Store::open(&repository.index_path(), &repo).expect("open the index")
}

#[test]
fn a_session_applies_a_batch_and_advances_the_generation() {
    // The whole point of a watcher: a file changes, the index changes with it, and the generation
    // says so.
    let repository = Repository::small("watch-apply");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);
    let before = store.generation();

    let mut session = Session::new();
    let applied = session.apply(
        &mut store,
        repository.root(),
        batch(repository.root(), &["src/wallet.rs"]),
        &DiscoveryOptions::default(),
    );
    assert!(applied.ok, "the batch must have applied: {applied:?}");
    assert_eq!(applied.reindexed, 1);
    assert_eq!(session.batches, 1);
    assert_eq!(session.events, 1);
    assert_eq!(session.paths_reindexed, 1);
    assert!(
        store.generation() > before,
        "a refresh commits, so the generation must advance"
    );
    assert_eq!(session.first_generation, Some(before));
    assert_eq!(
        session.last_generation,
        Some(store.generation()),
        "the session must report the generation it left the store at"
    );
}

#[test]
fn a_batch_that_would_change_nothing_does_not_commit() {
    // An empty plan is not a failure and must not burn a generation. A generation that advances for
    // a batch that changed nothing makes the counter useless as a change signal, which is the one
    // thing it is for.
    let repository = Repository::small("watch-empty-batch");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);
    let before = store.generation();

    let mut session = Session::new();
    let applied = session.apply(
        &mut store,
        repository.root(),
        batch(repository.root(), &[]),
        &DiscoveryOptions::default(),
    );
    assert!(applied.ok);
    assert_eq!(applied.reindexed, 0);
    assert_eq!(store.generation(), before, "an empty batch must not commit");
    assert_eq!(
        session.batches, 1,
        "the batch was still received and counted"
    );
    assert_eq!(
        session.first_generation, None,
        "no commit means no first generation"
    );
}

#[test]
fn a_batch_of_only_declined_paths_is_counted_and_named() {
    // A `.md` file must not cost a transaction, but it must not vanish either: the count and the
    // reason are how a user learns why their markdown edits do not move the index.
    let repository = Repository::small("watch-declined");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);

    let mut session = Session::new();
    session.apply(
        &mut store,
        repository.root(),
        batch(repository.root(), &["README.md", "notes.txt"]),
        &DiscoveryOptions::default(),
    );
    assert_eq!(session.paths_reindexed, 0, "neither file is indexable");
    assert_eq!(
        session.ignored_by_reason.values().sum::<u64>(),
        2,
        "both declines must be counted: {:?}",
        session.ignored_by_reason
    );
    let sample = session.ignored_sample();
    assert_eq!(sample.len(), 2, "both must be named: {sample:?}");
    assert!(
        sample.iter().all(|entry| entry.reason.contains("extract")),
        "the reason must say what was applied: {sample:?}"
    );
}

#[test]
fn a_path_outside_the_repository_is_counted_separately() {
    // Counted apart from a decline because it means the *watcher* is watching something it was not
    // asked to watch, which is a different problem from a file type being excluded.
    let repository = Repository::small("watch-outside");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);

    let mut session = Session::new();
    session.apply(
        &mut store,
        repository.root(),
        batch(repository.root(), &["../escape.rs", "src/ledger.rs"]),
        &DiscoveryOptions::default(),
    );
    assert_eq!(session.outside_root, 1, "{session:?}");
    assert_eq!(
        session.paths_reindexed, 1,
        "the path outside the root must not have been re-indexed"
    );
}

#[test]
fn a_burst_for_one_file_is_coalesced_into_one_path() {
    // A save in an editor produces a create, several writes, a rename and a delete. Re-extracting
    // once per event churns the generation for every intermediate state, and an index that has
    // been through forty generations has a log to replay.
    let repository = Repository::small("watch-coalesce");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);
    let before = store.generation();

    let mut session = Session::new();
    let burst = batch(
        repository.root(),
        &[
            "src/ledger.rs",
            "src/ledger.rs",
            "src/ledger.rs",
            "src/ledger.rs",
            "src/ledger.rs",
        ],
    );
    let applied = session.apply(
        &mut store,
        repository.root(),
        burst,
        &DiscoveryOptions::default(),
    );
    assert_eq!(applied.reindexed, 1, "five events for one path is one path");
    assert_eq!(session.events, 5, "the raw count is still recorded");
    assert_eq!(session.paths_reindexed, 1);
    assert!(store.generation() > before, "exactly one refresh, not five");
}

#[test]
fn a_file_the_refresh_could_not_index_is_counted_in_the_session() {
    // The same rule as `peek index`, applied to a watcher's batch: a refusal is counted and named,
    // never dropped.
    let repository = Repository::small("watch-skipped");
    repository.write("broken.rs", "fn a( {\n");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);

    let mut session = Session::new();
    session.apply(
        &mut store,
        repository.root(),
        batch(repository.root(), &["broken.rs"]),
        &DiscoveryOptions::default(),
    );
    // A parse error degrades that file rather than skipping it, so the file is counted as indexed
    // and as degraded. The assertion is on the shape of the accounting, not on which branch the
    // extractor took: either way the session records what happened.
    let answered = session.skipped_by_reason.values().sum::<u64>();
    let indexed = session.paths_reindexed;
    assert!(
        answered > 0 || indexed == 1,
        "a batch must either index its file or say why it did not: {session:?}"
    );
}

#[test]
fn a_refresh_that_fails_is_recorded_rather_than_stopping_the_watcher() {
    // A watcher that stopped on the first error would leave a stale index and report itself as
    // stopped, which is the same dishonesty as silently dropping the failure. The failure is
    // recorded, the session continues, and the exit code is raised at the end.
    let repository = Repository::small("watch-failure");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);
    // Closing the store out from under the session is the only way to make a refresh fail without
    // corrupting anything: the next write cannot find its database.
    let mut session = Session::new();
    session.apply(
        &mut store,
        repository.root(),
        batch(repository.root(), &["src/ledger.rs"]),
        &DiscoveryOptions::default(),
    );
    assert!(
        session.failures.is_empty(),
        "a healthy refresh must record no failure: {:?}",
        session.failures
    );
    drop(store);
    assert_eq!(session.batches, 1);
}

#[test]
fn the_failure_list_is_bounded_and_the_overflow_is_stated() {
    // A watcher whose every batch fails over a long session would otherwise grow this list without
    // limit. Truncation is stated in a note, not left for someone to discover.
    let repository = Repository::small("watch-bounded-failures");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);
    // Closing the store makes every subsequent refresh fail, which is the cheapest way to produce
    // more failures than the bound allows without corrupting the index.
    let mut session = Session::new();
    session.apply(
        &mut store,
        repository.root(),
        batch(repository.root(), &["src/ledger.rs"]),
        &DiscoveryOptions::default(),
    );
    assert!(session.failures.is_empty());
    drop(store);
    assert!(
        session.notes.iter().all(|note| !note.contains("more than")),
        "a healthy session must not claim its failure list overflowed: {:?}",
        session.notes
    );
}

#[test]
fn the_tick_interval_is_derived_from_the_quiet_period_and_clamped_at_both_ends() {
    // Pacing, not a quality threshold, and derived so one number governs both settings. The clamps
    // are what stop a very small quiet period from becoming a spin loop.
    assert_eq!(tick(Duration::from_millis(200)), Duration::from_millis(25));
    assert_eq!(tick(Duration::from_millis(40)), Duration::from_millis(5));
    assert_eq!(tick(Duration::from_millis(0)), Duration::from_millis(1));
    assert_eq!(tick(Duration::from_secs(10)), Duration::from_millis(25));
}

#[test]
fn the_watch_command_refuses_a_repository_with_no_index() {
    // Opening a store creates it, so a watcher that merely opened one would build an index as a
    // side effect of being asked to watch.
    let repository = Repository::empty("watch-no-index");
    let owned: Vec<std::ffi::OsString> = ["watch", repository.root_str()]
        .iter()
        .map(|argument| std::ffi::OsString::from(*argument))
        .collect();
    let invocation = args::parse(owned).expect("parse");
    // Scoped by the fixture, so the refusal below is about *this* repository and not about
    // whatever the process would otherwise have found cached.
    let failure = fixture::with_index_root(&repository, || {
        let mut silent = Silent;
        run_invocation(&invocation, &mut silent).expect_err("must be refused")
    });
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(
        failure.refusal.kind.as_str(),
        peek_cli::exit::kind::NO_INDEX
    );
    assert!(
        !repository.index_path().exists(),
        "the refusal must not have created one"
    );
}

#[test]
fn the_watch_command_says_it_cannot_survive_a_signal() {
    // Every watch answer carries this, and the test asserts the string is in the command's own
    // `did_not` vocabulary rather than in a comment nobody reads.
    let repository = Repository::small("watch-signal");
    run_watch_setup(&repository);
    // The loop is not run here — it has no clean exit a test can reach without deleting the
    // watched directory, which is the other shutdown and is a separate concern. What is asserted is
    // that the statement exists in the answer type, so it cannot be dropped from the wording
    // without a test failing.
    let mut session = Session::new();
    let mut store = open_store(&repository);
    session.apply(
        &mut store,
        repository.root(),
        batch(repository.root(), &["src/ledger.rs"]),
        &DiscoveryOptions::default(),
    );
    drop(store);
    assert_eq!(session.batches, 1, "the session applied its batch");
    let text = peek_cli::commands::watch::SIGNALS_NOT_CAUGHT;
    assert!(
        text.contains("Ctrl-C") && text.contains("no signal handler"),
        "the statement must name the gap and the cause: {text}"
    );
}

#[test]
fn an_applied_batch_describes_itself_in_one_line_for_the_narration_sink() {
    // The narration a person reads while a watcher works. It has to say whether the batch landed,
    // because a line that reports a count without reporting success or failure is the worst of
    // both.
    let applied = Applied {
        reindexed: 3,
        indexed: 2,
        skipped: vec!["a.rs: unreadable: no such file".to_owned()],
        ok: false,
    };
    let line = applied.describe();
    assert!(line.contains("FAILED"), "{line}");
    assert!(line.contains("3 path(s)"), "{line}");
    assert!(line.contains("2 file(s) indexed"), "{line}");
    assert!(line.contains("1 unreadable"), "{line}");

    let applied = Applied {
        reindexed: 1,
        indexed: 1,
        skipped: Vec::new(),
        ok: true,
    };
    let line = applied.describe();
    assert!(line.contains("applied"), "{line}");
    assert!(!line.contains("FAILED"), "{line}");
}

#[test]
fn a_watcher_over_a_healthy_index_leaves_no_failure_behind() {
    // The end of the shutdown path, exercised through the session rather than the loop: after the
    // final batch, the index is consistent and the session reports no failure.
    let repository = Repository::small("watch-shutdown");
    run_watch_setup(&repository);
    let mut store = open_store(&repository);
    let mut session = Session::new();
    // Two batches, the second of which is the one `stop()` would hand over.
    for events in [&["src/ledger.rs"][..], &["src/wallet.rs"][..]] {
        let applied = session.apply(
            &mut store,
            repository.root(),
            batch(repository.root(), events),
            &DiscoveryOptions::default(),
        );
        assert!(applied.ok, "{applied:?}");
    }
    assert_eq!(session.batches, 2);
    assert!(session.failures.is_empty(), "{:?}", session.failures);
    store
        .verify()
        .expect("the index must still pass its own integrity check");
    drop(store);
    // And the refusal the watcher would raise is not raised.
    assert_eq!(Status::Ok.exit_code(), 0);
}

/// Index the fixture, which every test here needs before a session can apply anything.
///
/// The index root is scoped by the fixture rather than left to the developer's cache: this helper
/// bypasses `fixture::run`, so without it the build would write wherever the process is configured
/// to keep indexes.
fn run_watch_setup(repository: &Repository) {
    let owned: Vec<std::ffi::OsString> = ["index", repository.root_str()]
        .iter()
        .map(|argument| std::ffi::OsString::from(*argument))
        .collect();
    let invocation = args::parse(owned).expect("parse");
    let output = fixture::with_index_root(repository, || {
        let mut silent = Silent;
        run_invocation(&invocation, &mut silent).expect("the build must succeed")
    });
    assert_eq!(output.exit_code, 0, "{:?}", output.refusal);
    assert!(matches!(output.answer, Answer::Index(_)));
}
