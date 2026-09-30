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
use peek_cli::exit::{self, Status, kind};
use peek_cli::{Mode, Output, render};

/// Assert that `output` is a command that declined, with the given reason, in both modes.
///
/// `what` names the case in every failure message, because the same assertion runs for a bad
/// command line, a refusal and an engine failure, and a message that does not say which one was
/// wrong is a message that costs a run to interpret.
fn assert_declined(output: &Output, status: Status, code: u8, reason: &str, what: &str) {
    assert_eq!(output.status, status, "{what}: the wrong status");
    assert_eq!(output.exit_code, code, "{what}: the wrong exit code");
    assert_eq!(
        output.answer.command(),
        "declined",
        "{what}: the answer kind"
    );
    let answer = match &output.answer {
        Answer::Declined(answer) => answer,
        other => panic!("{what}: expected a declined answer, got {other:?}"),
    };
    assert_eq!(answer.status, status, "{what}: the answer's status");
    assert_eq!(answer.command, output.command, "{what}: the command named");
    assert_eq!(
        answer.refusal.kind.as_str(),
        reason,
        "{what}: the wrong reason"
    );
    assert!(
        !answer.refusal.message.is_empty(),
        "{what}: a refusal with no words"
    );
    assert!(
        !answer.did_not.is_empty(),
        "{what}: an empty list claims completeness"
    );
    assert_eq!(
        output.refusal.as_ref(),
        Some(&answer.refusal),
        "{what}: two refusals"
    );
    assert_eq!(output.root, "", "{what}: it read no repository");

    // The human mode, which was already right. Asserted anyway, because "the reason is stated once"
    // is a new property: the envelope used to append the refusal after the answer, so a declined
    // output would have said the same thing twice.
    let text = output.render();
    let once = text.matches(answer.refusal.message.as_str()).count();
    assert_eq!(
        once, 1,
        "{what}: the reason is not stated exactly once:\n{text}"
    );
    assert!(
        text.contains(reason),
        "{what}: the reason is not named:\n{text}"
    );
    assert!(
        !text.contains("commands:"),
        "{what}: rendered as the list:\n{text}"
    );
    assert!(
        text.contains("could not:"),
        "{what}: no did_not block:\n{text}"
    );

    // The JSON mode, parsed. Every claim above is also a claim about this document, and none of
    // them is about a string.
    let json = render(output, &Mode::json());
    let value: serde_json::Value = serde_json::from_str(&json).expect(what);
    assert_eq!(value["command"], output.command, "{what}: the envelope");
    assert_eq!(value["status"], status.as_str(), "{what}: the status");
    assert_eq!(value["exit_code"], code, "{what}: the exit code");
    assert_eq!(
        value["answer"]["command"], "declined",
        "{what}: the answer tag"
    );
    assert_eq!(
        value["answer"]["command_name"], output.command,
        "{what}: the command named"
    );
    assert_eq!(
        value["answer"]["status"],
        status.as_str(),
        "{what}: the answer's status"
    );
    let carried = &value["answer"]["refusal"];
    assert_eq!(carried["kind"], reason, "{what}: not where a machine looks");
    let message = answer.refusal.message.as_str();
    assert_eq!(carried["message"], message, "{what}: the message");
    assert_eq!(
        value["refusal"]["kind"], reason,
        "{what}: the envelope's refusal"
    );
    let listed = value["answer"].to_string();
    assert!(
        !listed.contains("commands:"),
        "{what}: the JSON is the list"
    );
    if let Some(minimum) = answer.refusal.minimum_tokens {
        assert_eq!(
            carried["minimum_tokens"], minimum,
            "{what}: the floor must reach a machine"
        );
    }

    // And the mode reads back into the value it was rendered from, which is what makes "parsed
    // with serde_json" worth doing rather than decorative.
    let parsed: Output = serde_json::from_str(&json).expect(what);
    assert_eq!(&parsed, output, "{what}: the round trip lost something");
}

#[test]
fn a_command_line_that_was_not_understood_carries_the_reason_as_its_answer() {
    // Exit 2. Four ways to write a command line that cannot be read, because "every argument error
    // exits 2" is only a useful claim if the reason travels with it.
    let repository = Repository::small("failure-usage");

    for (argv, reason, command) in [
        (vec!["status", "--nonsense"], kind::UNKNOWN_FLAG, "status"),
        (vec!["contex"], kind::UNKNOWN_COMMAND, "contex"),
        (vec!["context", "--budget"], kind::MISSING_VALUE, "context"),
        (vec!["status", "extra"], kind::WRONG_ARITY, "status"),
    ] {
        let output = run(&repository, &argv).output;
        let what = format!("{argv:?}");
        assert_declined(&output, Status::Usage, exit::EXIT_USAGE, reason, &what);
        // A line the parser rejected has no invocation to take the command from, so the name is
        // recovered from the arguments — and an unknown command is reported as it was typed rather
        // than as the command it probably meant.
        assert_eq!(output.command, command, "{argv:?}: the wrong command name");
    }
}

#[test]
fn a_refusal_carries_the_reason_as_its_answer() {
    // Exit 3, and the case the finding was reported on: a budget too small to hold the report that
    // would say what was dropped. The floor travels with it, so one round trip is enough.
    //
    // **The repository is indexed first, and that is load-bearing.** `peek context` opens the store
    // before it looks at the budget, so a repository that was never indexed refuses with
    // `no_index` and the budget is never reached — the sibling test below is exactly that case, on
    // its own fixture because the process-global index root means one repository at a time. This
    // test is about the *budget* refusal, so it has to give the command an index to refuse against.
    let repository = Repository::small("failure-refused");
    run(&repository, &["index", repository.root_str()]);
    let root = repository.root_str();
    let argv = ["context", "wallet_charge", "--budget", "1", "--root", root];
    let output = run(&repository, &argv).output;
    let what = "a budget below the report floor";
    assert_declined(
        &output,
        Status::Refused,
        exit::EXIT_REFUSED,
        kind::BUDGET_TOO_SMALL,
        what,
    );
    let answer = match &output.answer {
        Answer::Declined(answer) => answer,
        other => panic!("expected a declined answer, got {other:?}"),
    };
    let floor = answer
        .refusal
        .minimum_tokens
        .expect("a refused budget must state the floor that would have worked");
    assert!(
        floor > 1,
        "the floor must exceed the refused budget: {floor}"
    );
}

#[test]
fn a_refusal_for_a_missing_index_carries_its_own_reason() {
    // The other refusal, and the reason the reason is carried rather than implied: "your budget is
    // too small" and "this repository was never indexed" are different problems with different
    // fixes, and a caller that cannot tell them apart retries or gives up for the wrong reason.
    //
    // Its own test because the fixture serialises on a process-global lock: one repository at a
    // time, so a test cannot hold two.
    let repository = Repository::empty("failure-no-index");
    let output = run(&repository, &["status", "--root", repository.root_str()]).output;
    let what = "a repository with no index";
    assert_declined(
        &output,
        Status::Refused,
        exit::EXIT_REFUSED,
        kind::NO_INDEX,
        what,
    );
    let absent = repository.index_path();
    assert!(!absent.exists(), "the refusal created the index it refused");
}

#[test]
fn an_engine_failure_carries_the_reason_as_its_answer() {
    // Exit 1, and the one that matters most for a script: the index is broken, so every command
    // that opens it must say so rather than answer with zeros or with a help document.
    let repository = Repository::small("failure-engine");
    run(&repository, &["index", repository.root_str()]);
    let broken = b"this is not a database, it is a sentence about one";
    std::fs::write(repository.index_path(), broken).expect("corrupt the index");
    let output = run(&repository, &["status", "--root", repository.root_str()]).output;
    let what = "a corrupted index";
    assert_declined(
        &output,
        Status::Failed,
        exit::EXIT_FAILED,
        kind::ENGINE,
        what,
    );
}

#[test]
fn a_command_that_worked_never_carries_a_declined_answer() {
    // The other half, and the one a change that only ever adds the variant would miss: a successful
    // command must not start reporting a refusal, or the exit code would still be zero and a
    // caller would learn to distrust the reason.
    let repository = Repository::small("failure-successful");
    run(&repository, &["index", repository.root_str()]);
    let root = repository.root_str();
    // An absolute path, deliberately, and no longer for the reason it was first written: a
    // *relative* one was a separate open defect about which tree a path inside a `--root` command
    // is resolved against, and mixing the two in here would have made a failure of either
    // impossible to read. That defect is fixed and the path resolution has its own tests, so this
    // stays absolute to keep the test about the answer type.
    let file = repository.root().join("src/ledger.rs");
    let file = file.to_str().expect("a temporary path is UTF-8");

    for (argv, answer_kind) in [
        (vec!["status", "--root", root], "status"),
        (vec!["doctor", "--root", root], "doctor"),
        (vec!["explain", "wallet_charge", "--root", root], "explain"),
        (vec!["callers", "wallet_charge", "--root", root], "walk"),
        (vec!["callees", "wallet_charge", "--root", root], "walk"),
        (vec!["dependents", "wallet_charge", "--root", root], "walk"),
        (
            vec![
                "context",
                "wallet_charge",
                "--budget",
                "20000",
                "--root",
                root,
            ],
            "context",
        ),
        (vec!["rm", file, "--root", root], "rm"),
    ] {
        let output = run(&repository, &argv).output;
        let refusal = &output.refusal;
        assert_eq!(output.exit_code, 0, "{argv:?} did not succeed: {refusal:?}");
        assert_eq!(
            output.answer.command(),
            answer_kind,
            "{argv:?}: the answer kind"
        );
        assert!(
            !output.answer.states_its_refusal(),
            "{argv:?}: it reports a refusal"
        );
        assert!(
            !output.answer.did_not().is_empty(),
            "{argv:?}: claims to be complete"
        );
    }

    // The two commands that read nothing, so they have no repository to name and cannot be held to
    // the same shape — but which must not be a declined answer either, or `peek --help` would be a
    // failure wearing a success code. Spelled as the flags this time, in both forms, because the
    // flag table lists them and the parser now reads them; `peek --version` used to print the
    // help, and a test that spelled the word instead of the flag is a test that cannot catch it
    // coming back.
    for (argv, answer_kind) in [
        (vec!["--help"], "help"),
        (vec!["-h"], "help"),
        (vec!["--version"], "version"),
        (vec!["-V"], "version"),
    ] {
        let output = run(&repository, &argv).output;
        assert_eq!(output.exit_code, 0, "{argv:?} must succeed");
        assert_eq!(
            output.answer.command(),
            answer_kind,
            "{argv:?}: the answer kind"
        );
    }
    // And the flag is not the word with a different label on it. The version answer names the
    // schema it read; the help answer does not and must not, because a `--version` that printed the
    // help would otherwise satisfy every assertion above while answering a different question.
    let version = run(&repository, &["--version"]).output.answer.render();
    let help = run(&repository, &["--help"]).output.answer.render();
    assert!(
        version.contains("schema v"),
        "--version must name the schema it read: {version}"
    );
    assert!(
        help.contains("usage:"),
        "the help must be the help, not the version: {help}"
    );
    assert!(
        !help.contains("schema v"),
        "the help must not be a version answer wearing the help's label: {help}"
    );
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
        declined.root.is_empty(),
        "a decline names a tree it never read"
    );
    // **The negation that was here asserted the opposite of the message above it**, and so of the
    // behaviour `declined` documents: "`root` and `index_path` are empty and `index_existed` is
    // `false` — a command that declined did not read a tree, and naming the one it was pointed at
    // would be the same defect as naming an answer it never produced". A decline reached no
    // repository, so it names no repository *and* no index; the two fields are the same claim and
    // are held to it together. Asserting the index path was non-empty would have required the
    // defect this file exists to prevent.
    assert!(
        declined.index_path.is_empty(),
        "a decline names an index it never read"
    );
    assert_eq!(
        worked.root,
        repository.root_str(),
        "a success names the tree it read"
    );
    assert!(
        !worked.index_path.is_empty(),
        "a success names the index it wrote"
    );
    let said_no = declined.answer.states_its_refusal();
    let said_yes = worked.answer.states_its_refusal();
    assert_ne!(said_no, said_yes, "exactly one of these produced no answer");
}
