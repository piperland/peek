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

/// The marker an ungenerated golden carries, so it fails loudly rather than comparing equal.
const UNGENERATED: &str = "PEEK_UPDATE_GOLDENS";

/// Whether the caller asked for the goldens to be rewritten.
fn updating() -> bool {
    std::env::var_os("PEEK_UPDATE_GOLDENS").is_some_and(|value| !value.is_empty())
}

/// Where a golden lives, both to be read and to be written.
///
/// **One function, because the two were two.** The write path originally joined the bare name while
/// the read went through `include_str!` on `"golden/<name>.txt"`, so
/// `PEEK_UPDATE_GOLDENS=1 cargo test --test golden` wrote `golden/context-complete` and every
/// subsequent run still read the 42-byte placeholder at `golden/context-complete.txt`. The
/// generation appeared to succeed, the test still failed, and the directory filled with a second
/// file nobody had committed. A write path and a read path that disagree are worse than neither:
/// the first run looks like progress and every run after it looks like the same failure.
///
/// The extension is derived rather than repeated, so a call site that names `context-complete`
/// cannot read one file and write another.
fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.txt"))
}

/// Read a golden from disk.
///
/// **At run time, not with `include_str!`.** The goldens were embedded at compile time, so
/// `PEEK_UPDATE_GOLDENS=1 cargo test` wrote the file and then compared against the value baked into
/// the binary that was already running — the placeholder. Generating a golden therefore took two
/// passes, and the first one looked like it had worked. Reading at run time makes the file on disk
/// the only copy, so one pass writes it and the next one verifies it, which is what the flag's
/// documentation already claimed.
fn read_golden(name: &str) -> String {
    let path = golden_path(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the golden for {name} could not be read from {}: {error}\n\
             generate it with `PEEK_UPDATE_GOLDENS=1 cargo test --test golden`",
            path.display()
        )
    })
}

/// Compare a rendering against a golden, or write it when asked to.
///
/// The write path is the only way a golden changes, and it is behind an environment variable so
/// that regenerating one is a deliberate act rather than something a test run does by accident.
fn assert_golden(name: &str, actual: &str) {
    if updating() {
        std::fs::write(golden_path(name), format!("{actual}\n")).expect("write the golden");
        return;
    }
    let expected = read_golden(name);
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
///
/// Through `fixture::run_direct`, which is what scopes the index root: without it the command
/// would resolve its index wherever the process is configured to keep one, and a golden would be
/// generated against a different tree.
fn render(repository: &Repository, argv: &[&str]) -> String {
    let mut silent = Silent;
    match fixture::run_direct(repository, argv, &mut silent) {
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
fn run_any(repository: &Repository, argv: &[&str]) -> peek_cli::Output {
    // A refusal comes back as `Err(Failure)`, not as an `Output` with a status — the two are
    // unified in the binary's `main`, and this helper is not `main`. So a test that wants to
    // inspect a *refused* answer uses `fixture::run`, which is the one place the two
    // representations meet. Reaching for it here and being surprised by an `Err` is the mistake
    // this comment exists to prevent.
    let mut silent = Silent;
    let outcome = fixture::run_direct(repository, argv, &mut silent);
    outcome.expect("the command must produce an output")
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
fn the_index_the_fixture_wrote_is_the_one_a_direct_call_reads() {
    // **The disagreement that failed two of the tests below.** The `index` command in those tests
    // went through a helper that scoped the index root, so it wrote into this fixture's cache.
    // The `context` command that followed was handed straight to `peek_cli::run` with no scoping,
    // so it resolved the index from the process default — where an identity derived from a
    // temporary directory that did not exist a minute ago cannot have one. It refused with
    // `no_index`, which is also exit 3 and also an `Err`, so the only thing that disagreed was the
    // kind and both tests reported a budget defect instead.
    //
    // `--root` would not have told you. Every one of those command lines already named the
    // repository, and the root is not what decides where the index is: `--index-dir` is read by
    // the binary's `main` and not by `peek_cli::run`, so a library caller has one lever on the
    // index root and it is the process-global override the fixture installs.
    //
    // Asserted in both directions, because either half alone would pass for the wrong reason.
    let repository = Repository::small("golden-index-agreement");
    fixture::run(&repository, &["index"]);
    let written = repository.index_path();
    assert!(
        written.is_file(),
        "the fixture's own build must have written {written:?}"
    );

    // The positive half: a direct call, scoped the way every helper here scopes one, resolves and
    // reads exactly that file. Compared as a path rather than as an answer type, because the answer
    // type is what two wrong index roots can both produce.
    let found = run_any(
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
    assert_eq!(
        found.index_path,
        written.display().to_string(),
        "a direct call must read the very file the fixture wrote"
    );
    assert!(
        matches!(&found.answer, Answer::Context(_)),
        "the direct call must have found that index rather than refused: {:?}",
        found.refusal
    );

    // The negative half, and the reason the positive half is worth anything: the same command line
    // with the fixture's scoping left off resolves somewhere else entirely. If this ever stopped
    // being true the fixture's cache had become the process default, every run in this file would
    // go on finding the index by accident, and the scoping would be untested rather than working.
    let owned: Vec<std::ffi::OsString> = [
        "context",
        "wallet_charge",
        "--budget",
        "20000",
        "--root",
        repository.root_str(),
    ]
    .iter()
    .map(|argument| std::ffi::OsString::from(*argument))
    .collect();
    let invocation = args::parse(owned).expect("parse");
    let mut silent = Silent;
    let unscoped = run_invocation(&invocation, &mut silent)
        .expect_err("an unscoped call cannot have found this repository's index");
    assert_eq!(
        unscoped.refusal.kind.as_str(),
        peek_cli::exit::kind::NO_INDEX,
        "a call that resolved the wrong index root must say so as a missing index: {unscoped:?}"
    );
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
    assert_golden("context-complete", &actual);
}

/// Render a command that is *expected* to fail, without asserting that it did not.
///
/// Separate from [`render`] because a golden for a refusal or a failed diagnosis has to pin the
/// output of a non-zero exit, and a helper that panics on any non-zero exit cannot record it. The
/// exit code is still checked by the caller; this only stops the helper from pre-empting it.
fn render_any(repository: &Repository, argv: &[&str]) -> String {
    let mut silent = Silent;
    match fixture::run_direct(repository, argv, &mut silent) {
        Ok(output) => output.render(),
        Err(failure) => failure.render(),
    }
}

/// The budget at which this repository's pack is reduced rather than complete, found by halving.
///
/// **Found, never written down, and shared by both tests that need one.** A hardcoded budget breaks
/// the moment the report reserve, the neighbourhood or the fixture changes, and it breaks
/// silently: a constant that lands above the floor and below the neighbourhood tests `complete`
/// under a name that says `reduced`. A budget below the floor is worse still, because that is a
/// *refusal* — exit 3, an error string, and no pack at all — so a test asserting on a refused
/// rendering while claiming to test a reduced one passes for the wrong reason.
///
/// **The condition is the one that was once inverted here and in the test beside it**, which is
/// what the finding that parked this golden was built on: a sweep that takes the first budget whose
/// pack is *not* reduced stops at the largest one, renders a `complete` pack, and then fails an
/// assertion that the rendering says `reduced`. A test that finds its input by the negative of the
/// property it is testing is not testing the property.
fn sweep_for_a_reduced_budget(repository: &Repository) -> u64 {
    let mut budget = 100_000u64;
    for _ in 0..24 {
        let answer = run_any(
            repository,
            &[
                "context",
                "wallet_charge",
                "--budget",
                &budget.to_string(),
                "--root",
                repository.root_str(),
            ],
        );
        if matches!(answer.answer, Answer::Context(ref a) if a.pack.budget.status == BudgetStatus::Reduced)
        {
            return budget;
        }
        budget /= 2;
    }
    panic!(
        "no budget in the sweep produced a reduced pack; the fixture is too small to omit"
    );
}

#[test]
fn a_reduced_context_pack_renders_exactly_as_committed_and_names_what_it_dropped() {
    // The case the whole rule about budgets exists for. A smaller budget must produce a smaller
    // answer that *says* it is smaller, and the golden pins both halves: the `dropped:` block and
    // the `status: reduced` line.
    let repository = Repository::small("golden-context-reduced");
    render(&repository, &["index", repository.root_str()]);
    let reduced_at = sweep_for_a_reduced_budget(&repository);

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

    // The **data** carries the claim, and that is what is asserted here: a reduced pack must say
    // what it left out, with a reason, before anything is said about how it is worded.
    let output = fixture::run(
        &repository,
        &[
            "context",
            "wallet_charge",
            "--budget",
            &reduced_at.to_string(),
            "--root",
            repository.root_str(),
        ],
    )
    .output;
    match &output.answer {
        Answer::Context(answer) => {
            assert_eq!(
                answer.pack.budget.status,
                BudgetStatus::Reduced,
                "the sweep found this budget by observing this status: {answer:?}"
            );
            assert!(
                !answer.pack.omitted.is_empty(),
                "a reduced pack must name what it left out, not merely be smaller: {answer:?}"
            );
            for omission in &answer.pack.omitted {
                assert!(
                    !omission.reason.as_str().is_empty(),
                    "an omission without a reason is a silent omission: {omission:?}"
                );
            }
        }
        other => panic!("expected a context answer, got {other:?}"),
    }

    // The wording, pinned whole. Whether the prose actually carries the units and edges it claims
    // to is `a_reduced_pack_renders_its_units_and_edges`, which asserts it against the data and so
    // survives a rewording; this is where the exact text lives, and a reword is a deliberate act
    // that shows up in a diff.
    assert_golden("context-reduced", &actual);
}

/// The prose of a reduced pack, compared against the data it was made from.
///
/// **This is the test the finding R-013 was parked against, and R-013 does not describe this
/// code.** It claimed a `Reduced` pack "renders as its title and nothing else". The path says
/// otherwise: `ContextAnswer::render` opens with `pack.render(true)`, and `ContextPack::render` is
/// the report followed by every unit and every edge, with no branch on the status anywhere in it.
/// The reproduction that seemed to show the loss had an inverted sweep — it took the first budget
/// whose pack was *not* reduced, rendered that, and asserted the text said `reduced`, which is
/// precisely what a `complete` pack correctly does not say. Correcting the sweep corrected the
/// test; the finding and the `#[ignore]` were never revisited, and a golden that nothing wrote
/// stayed on the list of files the suite required.
///
/// So the claim is asserted in the form that would have caught it had it been true, and in a form
/// that survives a rewording: **every unit and every edge the pack holds is in the text.** A golden
/// pins the words; this pins the link between the words and the data, which is the seam the finding
/// was about — the same seam as R-014, and the one a formatter is otherwise never compared across.
#[test]
fn a_reduced_pack_renders_its_units_and_edges() {
    let repository = Repository::small("golden-context-reduced-render");
    render(&repository, &["index", repository.root_str()]);
    let reduced_at = sweep_for_a_reduced_budget(&repository);
    let output = fixture::run(
        &repository,
        &[
            "context",
            "wallet_charge",
            "--budget",
            &reduced_at.to_string(),
            "--root",
            repository.root_str(),
        ],
    )
    .output;
    assert_eq!(
        output.exit_code,
        0,
        "a reduced pack is an answer with limits, not a refusal"
    );
    let Answer::Context(answer) = &output.answer else {
        panic!("expected a context answer, got {:?}", output.answer);
    };
    assert_eq!(
        answer.pack.budget.status,
        BudgetStatus::Reduced,
        "the sweep found this budget by observing this status: {answer:?}"
    );

    let text = output.render();
    assert!(
        text.contains("status: reduced"),
        "a pack that dropped something must say so: {text}"
    );
    assert!(
        text.contains("dropped:"),
        "a pack that dropped something must name what it dropped: {text}"
    );

    // The linkage, unit by unit and edge by edge. Rendered with the flag the CLI itself passes,
    // `ContextAnswer::render` calling `pack.render(true)`, so a line missing here is a line the
    // reader would not have seen rather than a difference in how this test asked for it.
    assert!(
        !answer.pack.units.is_empty(),
        "a reduced pack holding no units is an empty one whatever its status says: {answer:?}"
    );
    for unit in &answer.pack.units {
        let line = unit.render();
        assert!(
            text.contains(&line),
            "the pack holds a unit the rendering never prints: {line}\n{text}"
        );
        for edge in unit.render_edges(true) {
            assert!(
                text.contains(&edge),
                "the pack holds an edge the rendering never prints: {edge}\n{text}"
            );
        }
    }
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
    assert_golden("doctor-healthy", &actual);
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
    assert_golden("doctor-broken", &actual);
}

// Not a golden: the numbers depend on the fixture's size and would make the file brittle.
// Asserted on the statements instead, which is the point — the refusal names the minimum, and the
// answer says so rather than being a help document.
//
// # Two of the assertions this test used to make were not true, and why
//
// It was ignored because it failed, and its body asserted that the answer of a refused
// `peek context --budget 70` is an `Answer::Context` with `refused` set, an empty pack, and a
// rendering containing `status: insufficient` and `at least`. None of that is what this budget
// produces:
//
// * **There is no pack.** 70 is below the engine's report reserve, so `context` refuses before
//   calling the engine at all — that is the `budget_too_small` refusal this test still asserts, by
//   name. A `ContextAnswer` would have to describe a pack that was never compiled, and a
//   `ContextAnswer` with an empty pack is the *other* refusal: a budget large enough to hold the
//   report and too small for the target, which the engine does answer, with units, omissions and a
//   floor. `ContextAnswer::refused` is the flag for that one and it is set there.
// * **`status: insufficient` and `at least` come from `ContextPack::render`**, so they exist only
//   when a pack was compiled. The floor for *this* refusal is rendered by `Refusal::render`, which
//   says `the smallest budget that would have been accepted is N token(s)`, and that is what is
//   asserted below.
//
// So the test now asserts what this command actually produces, which is the thing the whole finding
// was about: the answer is the refusal, and it says why exactly once.
#[test]
fn a_refused_context_pack_prints_the_refusal_and_the_floor() {
    let repository = Repository::small("golden-context-refused");
    render(&repository, &["index", repository.root_str()]);
    // Through `fixture::run`, not `run_any`: a refusal comes back from the library as an
    // `Err(Failure)`, and only the place the two representations meet turns that into an `Output`
    // with a status. This is the one place the test needs the unified form, because it is
    // asserting on what the *caller* receives.
    let output = fixture::run(
        &repository,
        &[
            "context",
            "wallet_charge",
            "--budget",
            "70",
            "--root",
            repository.root_str(),
        ],
    )
    .output;

    assert_eq!(output.status, Status::Refused);
    assert_eq!(
        output.exit_code, 3,
        "a budget below the report floor is a refusal"
    );

    // The answer is the refusal. A budget of 70 cannot hold the report that would say what was
    // left out, so the engine never compiled a pack and there is nothing for a `ContextAnswer` to
    // describe — the whole result is the reason, and that is what the answer field now carries.
    let answer = match &output.answer {
        Answer::Declined(answer) => answer,
        other => panic!("expected the refusal as the answer, not the list: {other:?}"),
    };
    assert_eq!(
        answer.status,
        Status::Refused,
        "the answer states the envelope's status"
    );
    assert_eq!(answer.command, "context", "the answer names what declined");
    assert_eq!(
        answer.refusal.kind.as_str(),
        // The engine's own word, not the CLI's JSON tag. `Query::peek` refuses with
        // `BudgetTooSmall`, and a refusal that renames the reason it is reporting is a refusal
        // that has to be translated twice.
        peek_cli::exit::kind::BUDGET_TOO_SMALL
    );
    let minimum = answer
        .refusal
        .minimum_tokens
        .expect("a refused budget must state the floor that would have worked");
    assert!(
        minimum > 70,
        "the floor must exceed the budget refused: {minimum}"
    );
    assert!(
        !answer.did_not.is_empty(),
        "a failure states what it did not do: {answer:?}"
    );

    // One reason, carried once. The envelope and the answer hold the same refusal because they
    // are the same refusal, and the human rendering states it a single time — a reader who finds
    // the same paragraph twice has to work out which of the two was the answer.
    let envelope = output.refusal.as_ref().expect("a refusal must be carried");
    assert_eq!(
        envelope, &answer.refusal,
        "the envelope and the answer must not disagree"
    );
    let text = output.render();
    let once = text.matches(answer.refusal.message.as_str()).count();
    assert_eq!(once, 1, "the reason must be stated exactly once: {text}");
    let floor = minimum.to_string();
    assert!(
        text.contains("smallest budget"),
        "the floor must be printed: {text}"
    );
    assert!(
        text.contains(&floor),
        "the floor's number must be printed: {text}"
    );
    assert!(
        text.contains("could not:"),
        "what it did not do is missing: {text}"
    );
    assert!(
        !text.contains("commands:"),
        "a refusal must not be the list: {text}"
    );
    assert!(
        !text.contains("exit codes:"),
        "a refusal must not be the help: {text}"
    );
}

#[test]
fn a_context_refused_for_its_report_is_a_refusal_and_states_the_minimum() {
    // The other refusal: a budget too small to hold the report that would say what was dropped.
    // The message must carry the minimum so one round trip fixes it.
    let repository = Repository::small("golden-context-tiny");
    render(&repository, &["index", repository.root_str()]);
    // Through `run_direct`, so this command resolves the index the `index` command above wrote.
    // Handing the invocation to `peek_cli::run` unscoped resolves it from the process default
    // instead, where a repository that existed seconds ago cannot have an index — and the
    // refusal that comes back is `no_index`, which is still exit 3 and still an `Err`, so only
    // the kind was wrong and the failure read as a budget defect.
    let mut silent = Silent;
    let failure = fixture::run_direct(
        &repository,
        &[
            "context",
            "wallet_charge",
            "--budget",
            "1",
            "--root",
            repository.root_str(),
        ],
        &mut silent,
    )
    .expect_err("must be refused");
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
    let mut silent = Silent;
    let failure = fixture::run_direct(
        &repository,
        &["context", "wallet_charge", "--root", repository.root_str()],
        &mut silent,
    )
    .expect_err("must be refused");
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
    // vacuously, so its substance is asserted directly. **An ungenerated golden fails here**, and
    // that is the point of the test rather than an exception to it: this loop used to `continue`
    // past the marker, which meant a repository where no golden had ever been produced reported
    // every golden as present and non-trivial. The three comparisons above were already failing
    // for the same reason — but they failed on a *missing file*, one test at a time, and the test
    // whose entire job was to notice had skipped them. Regenerate with
    // `PEEK_UPDATE_GOLDENS=1 cargo test --test golden` and commit the result; there is no mode in
    // which a placeholder is the right content to commit.
    for name in [
        "context-complete",
        "context-reduced",
        "doctor-healthy",
        "doctor-broken",
    ] {
        let text = read_golden(name);
        assert!(
            !text.contains(UNGENERATED),
            "the golden for {name} has never been generated; run \
             `PEEK_UPDATE_GOLDENS=1 cargo test --test golden` and commit the result. A golden that \
             is missing must fail, not pass"
        );
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
