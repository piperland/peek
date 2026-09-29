//! The human output of the two commands whose exact wording is part of the product.
//!
//! # Why a golden file
//!
//! `context` is the namesake, and `doctor` is the contract's lifecycle check. Both are read by
//! people, and a reader who has learned to expect a line is entitled to find it. A test that
//! asserted only "the output contains `status: complete`" would pass for an output that also
//! contained a fabricated claim, dropped a note, or reordered the budget away from the top — every
//! one of which is the defect class this project exists to remove.
//!
//! So these tests compare the **whole** rendering, byte for byte, against a committed file. The
//! goldens are force-added (`git add -f`) because the repository ignores `*.txt`, and that ignore
//! rule is right for scratch files and wrong for a file a test fails on.
//!
//! # Regenerating a golden
//!
//! ```text
//! PEEK_UPDATE_GOLDENS=1 cargo test --test golden
//! ```
//!
//! With that variable set the tests **write** the rendering they produced and pass. Without it they
//! compare, and a mismatch fails with both texts. There is no mode in which an ungenerated golden
//! passes: a golden that does not exist is a failure, not a skip, because a skipped golden is a
//! golden nobody is checking.
//!
//! # What is compared, and what is not
//!
//! Byte for byte, with two substitutions, and both are named in [`normalise`]:
//!
//! * the repository's own path, which differs per run, and
//! * the index file's path, likewise.
//!
//! Everything else — the wording, the order, the numbers, the arithmetic — is compared exactly. A
//! formatter that starts inventing a number, dropping a line, or reordering the budget fails here.

// `expect` and `panic` are denied workspace-wide; an integration test is a separate crate and does
// not inherit the library's exemption. See `real_repository.rs` for the same justification.
#![allow(clippy::expect_used, clippy::panic)]

mod fixture;

use fixture::Repository;

use peek_cli::answer::Answer;
use peek_cli::exit::Status;
use peek_cli::progress::Silent;
use peek_cli::{args, run as run_invocation};
use peek_core::query::BudgetStatus;

/// The committed goldens.
const CONTEXT_COMPLETE: &str = include_str!("golden/context-complete.txt");
const CONTEXT_REDUCED: &str = include_str!("golden/context-reduced.txt");
const DOCTOR_HEALTHY: &str = include_str!("golden/doctor-healthy.txt");
const DOCTOR_BROKEN: &str = include_str!("golden/doctor-broken.txt");

/// The marker an ungenerated golden carries, so it fails loudly rather than comparing equal.
const UNGENERATED: &str = "PEEK_UPDATE_GOLDENS";

/// Whether the caller asked for the goldens to be rewritten.
fn updating() -> bool {
    std::env::var_os("PEEK_UPDATE_GOLDENS").is_some_and(|value| !value.is_empty())
}

/// Where a golden lives, so it can be written.
fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Compare a rendering against a golden, or write it when asked to.
///
/// The write path is the only way a golden changes, and it is behind an environment variable so
/// that regenerating one is a deliberate act rather than something a test run does by accident.
fn assert_golden(name: &str, expected: &str, actual: &str) {
    if updating() {
        std::fs::write(golden_path(name), format!("{actual}\n")).expect("write the golden");
        return;
    }
    if expected.contains(UNGENERATED) {
        panic!(
            "the golden for {name} has never been generated; run \
             `PEEK_UPDATE_GOLDENS=1 cargo test --test golden` and commit the result. A golden that \
             is missing must fail, not pass"
        );
    }
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "the human rendering of {name} changed.\n--- expected ---\n{}\n--- actual ---\n{actual}",
        expected.trim_end()
    );
}

/// Run a command line and render it, which is what the binary does.
fn render(_repository: &Repository, argv: &[&str]) -> String {
    let owned: Vec<std::ffi::OsString> = argv
        .iter()
        .map(|argument| std::ffi::OsString::from(*argument))
        .collect();
    let invocation = args::parse(owned).expect("the command line must parse");
    let mut silent = Silent;
    match run_invocation(&invocation, &mut silent) {
        Ok(output) => {
            assert_eq!(
                output.exit_code, 0,
                "{argv:?} exited {}: {:?}",
                output.exit_code, output.refusal
            );
            output.render()
        }
        Err(failure) => panic!("{argv:?} failed: {}", failure.render()),
    }
}

/// Run a command line and return the output whatever its status.
fn run_any(_repository: &Repository, argv: &[&str]) -> peek_cli::Output {
    let owned: Vec<std::ffi::OsString> = argv
        .iter()
        .map(|argument| std::ffi::OsString::from(*argument))
        .collect();
    let invocation = args::parse(owned).expect("the command line must parse");
    let mut silent = Silent;
    run_invocation(&invocation, &mut silent).expect("the command must produce an output")
}

/// Replace the two machine-dependent substrings with fixed placeholders.
///
/// **Exactly two**, and named in the failure message so a reader can see what was elided. Anything
/// else that varies per run is a defect in the output rather than an artefact of the test — an
/// elapsed time, a generation, a token count. Those are all deterministic for a fixed fixture,
/// which is why the fixture is committed rather than generated.
fn normalise(text: &str, repository: &Repository) -> String {
    text.replace(repository.root_str(), "<root>")
        .replace(&repository.index_path().display().to_string(), "<index>")
}

#[test]
fn a_complete_context_pack_renders_exactly_as_committed() {
    // The budget large enough that the neighbourhood is exhausted, which is the case a reader most
    // needs to trust: the pack is the whole answer.
    let repository = Repository::small("golden-context-complete");
    render(&repository, &["index", repository.root_str()]);
    let text = render(
        &repository,
        &[
            "context",
            "wallet_charge",
            "--budget",
            "20000",
            "--root",
            repository.root_str(),
        ],
    );
    let actual = normalise(&text, &repository);
    assert!(
        actual.contains("status: complete"),
        "a pack with unspent budget and no omissions must say the answer was complete: {actual}"
    );
    assert!(
        actual.contains("counted as: ceil(utf8 bytes / 3)"),
        "the counting rule must be printed so the arithmetic can be reproduced: {actual}"
    );
    assert_golden("context-complete", CONTEXT_COMPLETE, &actual);
}

/// Render a command that is *expected* to fail, without asserting that it did not.
///
/// Separate from [`render`] because a golden for a refusal or a failed diagnosis has to pin the
/// output of a non-zero exit, and a helper that panics on any non-zero exit cannot record it. The
/// exit code is still checked by the caller; this only stops the helper from pre-empting it.
fn render_any(_repository: &Repository, argv: &[&str]) -> String {
    let owned: Vec<std::ffi::OsString> = argv
        .iter()
        .map(|argument| std::ffi::OsString::from(*argument))
        .collect();
    let invocation = args::parse(owned).expect("the command line must parse");
    let mut silent = Silent;
    match run_invocation(&invocation, &mut silent) {
        Ok(output) => output.render(),
        Err(failure) => failure.render(),
    }
}

#[test]
fn a_reduced_context_pack_renders_exactly_as_committed_and_names_what_it_dropped() {
    // The case the whole rule about budgets exists for. A smaller budget must produce a smaller
    // answer that *says* it is smaller, and the golden pins both halves: the `dropped:` block and
    // the `status: reduced` line.
    let repository = Repository::small("golden-context-reduced");
    render(&repository, &["index", repository.root_str()]);

    // The budget is **found**, not written down. An earlier version of this test hardcoded 80,
    // which is below the engine's report reserve — so the command was refused rather than reduced
    // and the test was asserting on a refusal while claiming to test a reduced pack. A budget that
    // is below the floor is not a small budget, it is a rejected one, and the two are different
    // outputs with different meanings.
    //
    // So: start from a budget that certainly fits, and halve until the engine reports the pack as
    // reduced rather than complete. That is a sweep rather than a constant, so the golden keeps
    // testing what it names when the floor, the reserve or the neighbourhood changes.
    let mut budget = 100_000u64;
    let mut reduced_at = None;
    for _ in 0..24 {
        let answer = run_any(
            &repository,
            &[
                "context",
                "wallet_charge",
                "--budget",
                &budget.to_string(),
                "--root",
                repository.root_str(),
            ],
        );
        if !matches!(answer.answer, Answer::Context(ref a) if a.pack.budget.status == BudgetStatus::Reduced)
        {
            reduced_at = Some(budget);
            break;
        }
        budget = budget / 2;
    }
    let Some(reduced_at) = reduced_at else {
        panic!("no budget in the sweep produced a reduced pack; the fixture is too small to omit");
    };

    let text = render_any(
        &repository,
        &[
            "context",
            "wallet_charge",
            "--budget",
            &reduced_at.to_string(),
            "--root",
            repository.root_str(),
        ],
    );
    let actual = normalise(&text, &repository);
    assert!(
        actual.contains("status: reduced"),
        "a pack that dropped something must say so: {actual}"
    );
    assert!(
        actual.contains("dropped:"),
        "a pack that dropped something must name it: {actual}"
    );
    assert_golden("context-reduced", CONTEXT_REDUCED, &actual);
}

#[test]
fn a_healthy_doctor_report_renders_exactly_as_committed() {
    // The green path. A user who runs `doctor` to learn what was checked should get the same
    // answer every time, and the golden is what says so.
    let repository = Repository::small("golden-doctor-healthy");
    render(&repository, &["index", repository.root_str()]);
    let text = render(&repository, &["doctor", "--root", repository.root_str()]);
    let actual = normalise(&text, &repository);
    assert!(
        actual.contains("[pass]"),
        "the report must show what was checked: {actual}"
    );
    assert!(
        actual.contains("worst:"),
        "the report must end with the one line a reader takes away: {actual}"
    );
    assert_golden("doctor-healthy", DOCTOR_HEALTHY, &actual);
}

#[test]
fn a_broken_install_renders_exactly_as_committed() {
    // Contract L1. The report for a deliberately broken index is the output a user will read
    // when something is wrong, so its wording is pinned as firmly as the healthy one.
    let repository = Repository::small("golden-doctor-broken");
    render(&repository, &["index", repository.root_str()]);
    std::fs::write(
        repository.index_path(),
        b"not a database at all, only a sentence pretending to be one",
    )
    .expect("corrupt the index so that Store::open refuses it before reading a single row");
    let text = render_any(&repository, &["doctor", "--root", repository.root_str()]);
    let actual = normalise(&text, &repository);
    assert!(
        actual.contains("[fail]"),
        "a broken index must be reported as failing: {actual}"
    );
    assert_golden("doctor-broken", DOCTOR_BROKEN, &actual);
}

#[test]
fn a_refused_context_pack_prints_the_refusal_and_the_floor() {
    // Not a golden: the numbers depend on the fixture's size and would make the file brittle.
    // Asserted on the statements instead, which is the point — the refusal names the minimum, and
    // the answer says the pack is empty.
    let repository = Repository::small("golden-context-refused");
    render(&repository, &["index", repository.root_str()]);
    let output = run_any(
        &repository,
        &[
            "context",
            "wallet_charge",
            "--budget",
            "70",
            "--root",
            repository.root_str(),
        ],
    );
    assert_eq!(
        output.exit_code, 3,
        "a pack with no target in it is a refusal"
    );
    assert_eq!(output.status, Status::Refused);
    // Borrowed, not taken: `output` is rendered below, and moving the refusal out of it would
    // partially move the value the render borrows.
    let refusal = output.refusal.as_ref().expect("a refusal must be carried");
    assert_eq!(
        refusal.kind.as_str(),
        peek_cli::exit::kind::BUDGET_INSUFFICIENT
    );
    assert!(
        refusal.minimum_tokens.is_some(),
        "a refused budget must state the floor that would have worked: {refusal:?}"
    );
    match &output.answer {
        Answer::Context(answer) => {
            assert!(answer.refused, "the answer must say it was refused");
            assert!(answer.minimum_for_target.is_some(), "{answer:?}");
            assert!(
                answer.pack.units.is_empty(),
                "a refused pack contains nothing, and saying so is the answer"
            );
        }
        other => panic!("expected a context answer, got {other:?}"),
    }
    let text = output.render();
    assert!(text.contains("status: insufficient"), "{text}");
    assert!(
        text.contains("at least"),
        "the floor must be printed: {text}"
    );
}

#[test]
fn a_context_refused_for_its_report_is_a_refusal_and_states_the_minimum() {
    // The other refusal: a budget too small to hold the report that would say what was dropped.
    // The message must carry the minimum so one round trip fixes it.
    let repository = Repository::small("golden-context-tiny");
    render(&repository, &["index", repository.root_str()]);
    let owned: Vec<std::ffi::OsString> = [
        "context",
        "wallet_charge",
        "--budget",
        "1",
        "--root",
        repository.root_str(),
    ]
    .iter()
    .map(|argument| std::ffi::OsString::from(*argument))
    .collect();
    let invocation = args::parse(owned).expect("parse");
    let mut silent = Silent;
    let failure = run_invocation(&invocation, &mut silent).expect_err("must be refused");
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(
        failure.refusal.kind.as_str(),
        peek_cli::exit::kind::BUDGET_TOO_SMALL
    );
    let minimum = failure
        .refusal
        .minimum_tokens
        .expect("a budget refusal must state the minimum");
    assert!(
        minimum > 1,
        "the minimum must exceed the refused budget: {minimum}"
    );
    let rendered = failure.render();
    assert!(rendered.contains("smallest budget"), "{rendered}");
    assert!(rendered.contains(&minimum.to_string()), "{rendered}");
}

#[test]
fn a_context_with_no_budget_is_refused_rather_than_given_an_invented_one() {
    // The refusal a caller hits first, and the one that must carry the minimum: without it they
    // cannot know what to pass, and a default here would be a number nobody wrote down used to
    // size an answer.
    let repository = Repository::small("golden-context-no-budget");
    render(&repository, &["index", repository.root_str()]);
    let owned: Vec<std::ffi::OsString> =
        ["context", "wallet_charge", "--root", repository.root_str()]
            .iter()
            .map(|argument| std::ffi::OsString::from(*argument))
            .collect();
    let invocation = args::parse(owned).expect("parse");
    let mut silent = Silent;
    let failure = run_invocation(&invocation, &mut silent).expect_err("must be refused");
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(
        failure.refusal.kind.as_str(),
        peek_cli::exit::kind::NO_BUDGET
    );
    assert!(failure.refusal.minimum_tokens.is_some(), "{failure:?}");
    let rendered = failure.render();
    assert!(rendered.contains("--budget"), "{rendered}");
    assert!(rendered.contains("will not invent"), "{rendered}");
}

#[test]
fn the_golden_files_are_present_and_non_trivial() {
    // A golden that was accidentally committed empty would make every comparison above pass
    // vacuously, so its substance is asserted directly. The ungenerated marker is the one
    // exception, and `assert_golden` turns it into a failure rather than letting it through.
    for (name, text) in [
        ("context-complete", CONTEXT_COMPLETE),
        ("context-reduced", CONTEXT_REDUCED),
        ("doctor-healthy", DOCTOR_HEALTHY),
        ("doctor-broken", DOCTOR_BROKEN),
    ] {
        if text.contains(UNGENERATED) {
            continue;
        }
        assert!(
            text.lines().count() > 3,
            "the golden for {name} is too short to be a real rendering: {text:?}"
        );
        assert!(
            !text.contains("<root>"),
            "the {name} golden must not be a copy of the output"
        );
    }
}
