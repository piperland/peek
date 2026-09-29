//! A command that did not answer carries the refusal as its answer.
//!
//! # The defect this file is the regression test for
//!
//! `peek_cli::run` answers `Err(Failure)` for a usage error, a refusal and an engine failure,
//! because none of the three produced an answer. Turning that into an `Output` used to put the
//! **usage text** in the `answer` field, unconditionally, so the status and the exit code were
//! right, the refusal was attached to the envelope, and a person reading the terminal saw the
//! reason — while `--json`, a script and an agent, all of which read the answer, got the command
//! list where the reason belonged.
//!
//! It survived delivery review because the human output was right. It was wrong in the one place a
//! caller is a machine, and wrong in exactly the same way for three unrelated causes, so a failure
//! anywhere in the CLI looked like a success carrying a help document.
//!
//! # What is asserted, and where
//!
//! Every case is checked in **both** modes, and the JSON is parsed with `serde_json` and asserted
//! on structurally rather than compared as a string. The human mode is the one that was already
//! right, so asserting only there would test nothing. The assertions also cover the shape that was
//! missing entirely: a declined answer's `did_not` is never empty, because a failure carrying an
//! empty list is indistinguishable from a complete answer that happened to be short.

// `expect` and `panic` are denied workspace-wide; an integration test is a separate crate and does
// not inherit the library's exemption. See `real_repository.rs` for the same justification.
#![allow(clippy::expect_used, clippy::panic)]

mod fixture;

use fixture::{Repository, run};

use peek_cli::answer::Answer;
use peek_cli::exit::{Status, kind};
use peek_cli::{Mode, Output, render};

/// Assert that `output` is a command that declined, with the given reason, in both modes.
///
/// `what` names the case in every failure message, because the same assertion runs for a bad
/// command line, a refusal and an engine failure and a message that does not say which one was
/// wrong is a message that costs a run to interpret.
fn assert_declined(output: &Output, status: Status, code: u8, reason: &str, what: &str) {
    assert_eq!(output.status, status, "{what}: the wrong status");
    assert_eq!(output.exit_code, code, "{what}: the wrong exit code");
    assert_eq!(
        output.answer.command(),
        "declined",
        "{what}: the answer must be the refusal, not the command list"
    );
    let answer = match &output.answer {
        Answer::Declined(answer) => answer,
        other => panic!("{what}: expected a declined answer, got {other:?}"),
    };
    assert_eq!(
        answer.status, status,
        "{what}: the answer states a different status from the envelope"
    );
    assert_eq!(
        answer.refusal.kind.as_str(),
        reason,
        "{what}: the wrong reason"
    );
    assert!(
        !answer.refusal.message.is_empty(),
        "{what}: a refusal with no message tells the caller nothing"
    );
    assert!(
        !answer.did_not.is_empty(),
        "{what}: a command that did not answer must say what it did not do, or it reads as a \
         complete answer that happened to be short"
    );
    assert_eq!(
        output.refusal.as_ref(),
        Some(&answer.refusal),
        "{what}: the envelope and the answer must carry the same refusal, not two that can disagree"
    );
    assert_eq!(
        output.root, "",
        "{what}: a command that declined read no repository, so naming one would be a field that \
         looks like a measurement and is not"
    );

    // The human mode, which was already right. Asserted anyway, because "the reason is printed
    // once" is a new property: the envelope used to append the refusal after the answer, so a
    // declined output would have said the same thing twice.
    let text = output.render();
    assert_eq!(
        text.matches(answer.refusal.message.as_str()).count(),
        1,
        "{what}: the reason must be stated exactly once:\n{text}"
    );
    assert!(text.contains(reason), "{what}: the reason must be named:\n{text}");
    assert!(
        !text.contains("commands:") && !text.contains("exit codes:"),
        "{what}: a refusal must not render as the command list:\n{text}"
    );
    assert!(
        text.contains("could not:"),
        "{what}: what the command did not do must be printed too:\n{text}"
    );

    // The JSON mode, parsed. Every claim above is also a claim about this document, and none of
    // them is about a string.
    let json = render(output, &Mode::json());
    let value: serde_json::Value =
        serde_json::from_str(&json).unwrap_or_else(|error| panic!("{what}: {error} in\n{json}"));
    assert_eq!(value["command"], output.command, "{what}: the envelope");
    assert_eq!(value["status"], status.as_str(), "{what}: the status");
    assert_eq!(value["exit_code"], code, "{what}: the exit code");
    assert_eq!(
        value["answer"]["command"], "declined",
        "{what}: the answer's own tag"
    );
    assert_eq!(
        value["answer"]["command_name"], output.command,
        "{what}: the answer must name the command that declined"
    );
    assert_eq!(
        value["answer"]["status"], status.as_str(),
        "{what}: the answer's status"
    );
    assert_eq!(
        value["answer"]["refusal"]["kind"], reason,
        "{what}: the reason, where a machine looks for it"
    );
    assert_eq!(
        value["answer"]["refusal"]["message"], answer.refusal.message.as_str(),
        "{what}: the message"
    );
    assert_eq!(
        value["refusal"]["kind"], reason,
        "{what}: the envelope's refusal"
    );
    assert!(
        !value["answer"].to_string().contains("commands:"),
        "{what}: the JSON answer must not be the command list"
    );
    if let Some(minimum) = answer.refusal.minimum_tokens {
        assert_eq!(
            value["answer"]["refusal"]["minimum_tokens"], minimum,
            "{what}: the floor must reach a machine, or one round trip cannot fix it"
        );
    }

    // And the mode reads back into the value it was rendered from, which is what makes "parsed
    // with serde_json" worth doing rather than decorative.
    let parsed: Output =
        serde_json::from_str(&json).unwrap_or_else(|error| panic!("{what}: {error} in\n{json}"));
    assert_eq!(&parsed, output, "{what}: the round trip must be lossless");
}

#[test]
fn a_command_line_that_was_not_understood_carries_the_reason_as_its_answer() {
    // Exit 2. An unknown flag and an unknown command, because they are reported differently: the
    // first names the flag and the second names the command, and both are refusals rather than
    // answers.
    let repository = Repository::small("failure-usage");

    for (argv, reason, command) in [
        (vec!["status", "--nonsense"], kind::UNKNOWN_FLAG, "status"),
        (vec!["contex"], kind::UNKNOWN_COMMAND, "contex"),
        (vec!["context", "--budget"], kind::MISSING_VALUE, "context"),
        (vec!["status", "extra"], kind::WRONG_ARITY, "status"),
    ] {
        let output = run(&repository, &argv).output;
        assert_declined(
            &output,
            Status::Usage,
            peek_cli::exit::EXIT_USAGE,
            reason,
            &format!("{argv:?}"),
        );
        // A line the parser rejected has no invocation to take the command from, so the name is
        // recovered from the arguments — and the unknown command is reported as it was typed
        // rather than as the command it probably meant.
        assert_eq!(output.command, command, "{argv:?}: the wrong command name");
    }
}

#[test]
fn a_refusal_carries_the_reason_as_its_answer() {
    // Exit 3, and the case the finding was reported on: a budget too small to hold the report that
    // would say what was dropped. The floor travels with it, so one round trip is enough.
    let repository = Repository::small("failure-refused");
    let output = run(
        &repository,
        &[
            "context",
            "wallet_charge",
            "--budget",
            "1",
            "--root",
            repository.root_str(),
        ],
    )
    .output;
    assert_declined(
        &output,
        Status::Refused,
        peek_cli::exit::EXIT_REFUSED,
        kind::BUDGET_TOO_SMALL,
        "a budget below the report floor",
    );
    let answer = match &output.answer {
        Answer::Declined(answer) => answer,
        other => panic!("expected a declined answer, got {other:?}"),
    };
    let minimum = answer
        .refusal
        .minimum_tokens
        .expect("a refused budget must state the floor that would have worked");
    assert!(minimum > 1, "the floor must exceed the refused budget: {minimum}");}

#[test]
fn a_refusal_for_a_missing_index_carries_its_own_reason() {
    // The other refusal, and the reason the reason is carried rather than implied: "your budget is
    // too small" and "this repository was never indexed" are different problems with different
    // fixes, and a caller that cannot tell them apart retries or gives up for the wrong reason.
    //
    // Its own test because the fixture serialises on a process-global lock: one repository at a
    // time, so a test cannot hold two.
    let repository = Repository::empty("failure-no-index");
    assert_declined(
        &run(&repository, &["status", "--root", repository.root_str()]).output,
        Status::Refused,
        peek_cli::exit::EXIT_REFUSED,
        kind::NO_INDEX,
        "a repository with no index",
    );
    assert!(
        !repository.index_path().exists(),
        "the refusal must not have created the index it refused to report on"
    );
}

#[test]
fn an_engine_failure_carries_the_reason_as_its_answer() {
    // Exit 1, and the one that matters most for a script: the index is broken, so every command
    // that opens it must say so rather than answer with zeros or with a help document.
    let repository = Repository::small("failure-engine");
    run(&repository, &["index", repository.root_str()]);
    std::fs::write(
        repository.index_path(),
        b"this is not a database, it is a sentence about one",
    )
    .expect("corrupt the index so that opening it fails");
    assert_declined(
        &run(&repository, &["status", "--root", repository.root_str()]).output,
        Status::Failed,
        peek_cli::exit::EXIT_FAILED,
        kind::ENGINE,
        "a corrupted index",
    );
}

#[test]
fn a_command_that_worked_never_carries_a_declined_answer() {
    // The other half, and the one that would be missed by a change that only ever adds the
    // variant: a successful command must not start reporting a refusal, or the exit code would
    // still be zero and a caller would learn to distrust the reason.
    let repository = Repository::small("failure-successful");
    run(&repository, &["index", repository.root_str()]);
    let root = repository.root_str();
    let file = repository.root().join("src/ledger.rs");
    // An absolute path, deliberately: a *relative* one is a separate open defect about which
    // directory a path inside a `--root` command is resolved against, and mixing the two would
    // make a failure of either impossible to read.
    let file = file.to_str().expect("a temporary path is UTF-8");

    for (argv, answer_kind) in [
        (vec!["status", "--root", root], "status"),
        (vec!["doctor", "--root", root], "doctor"),
        (vec!["explain", "wallet_charge", "--root", root], "explain"),
        (vec!["callers", "wallet_charge", "--root", root], "walk"),
        (vec!["callees", "wallet_charge", "--root", root], "walk"),
        (vec!["dependents", "wallet_charge", "--root", root], "walk"),
        (
            vec!["context", "wallet_charge", "--budget", "20000", "--root", root],
            "context",
        ),
        (vec!["rm", file, "--root", root], "rm"),
    ] {
        let output = run(&repository, &argv).output;
        assert_eq!(
            output.exit_code, 0,
            "{argv:?} did not succeed, so it is not the case under test: {:?}",
            output.refusal
        );
        assert_eq!(
            output.answer.command(),
            answer_kind,
            "{argv:?}: a command that worked must carry its own answer"
        );
        assert!(
            !output.answer.states_its_refusal(),
            "{argv:?} succeeded and still reports that it declined"
        );
        assert!(
            !output.answer.did_not().is_empty(),
            "{argv:?} states nothing it could not do, which is claiming to be complete"
        );
    }

    // The two commands that read nothing, which have no repository to name and so cannot be held
    // to the same shape — but which must not be a declined answer either, or `peek --help` would
    // be a failure wearing a success code.
    for (argv, answer_kind) in [
        (vec!["--help"], "help"),
        // Spelled as the word, not the flag: the flag table lists `--version`, and the parser
        // never reads it, so `peek --version` answers with the help. That is a separate defect
        // and this test is not the place to change it — but it is why the word is used here.
        (vec!["version"], "version"),
    ] {
        let output = run(&repository, &argv).output;
        assert_eq!(output.exit_code, 0, "{argv:?} must succeed");
        assert_eq!(output.answer.command(), answer_kind, "{argv:?}");
    }
}

#[test]
fn a_command_that_declined_reached_no_repository() {
    // The discriminator between the two outcomes, and the reason it is here rather than inferred:
    // the fixture returns one value for both, so a test that wanted to be sure it was looking at a
    // decline has to be able to tell.
    let repository = Repository::small("failure-envelope");
    let declined = run(&repository, &["status", "--root", repository.root_str()]).output;
    let worked = run(&repository, &["index", repository.root_str()]).output;

    assert!(
        declined.root.is_empty() && declined.index_path.is_empty(),
        "a command that declined read no tree, so the envelope must not name one"
    );
    assert!(
        worked.root == repository.root_str() && !worked.index_path.is_empty(),
        "a command that worked names the tree it read and the index it wrote"
    );
    assert_ne!(
        declined.answer.states_its_refusal(),
        worked.answer.states_its_refusal(),
        "exactly one of these two produced no answer"
    );
}
