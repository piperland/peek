//! Behaviour of the argument table, the exit-code contract, and the answer types.
//!
//! Every test here names a claim in plain English and prints the state that contradicts it, so a
//! failure says what was wrong rather than only which assertion tripped.

// `expect` and `panic` are denied workspace-wide. An integration test is a separate crate and does
// not inherit the library's `cfg_attr(test, allow(..))`, so it is exempted here for the same reason
// `real_repository.rs` is: a panic inside a test means the claim being checked is false, and
// continuing would print a report built on an unverified fixture.
#![allow(clippy::expect_used, clippy::panic)]

mod fixture;

use std::path::Path;

use fixture::{Cleanup, Repository, run, run_verbatim};

use peek_cli::answer::{Answer, Counts, StatusAnswer};
use peek_cli::args::{self, Command, UsageError};
use peek_cli::exit::{self, Status, kind};
use peek_cli::{Mode, Output, render};

// ---------------------------------------------------------------------------
// The argument table
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_flag_is_refused_and_the_known_ones_are_listed() {
    // Audit D: `--kind nonsense` became no filter, silently. One error path, and it says what
    // would have been accepted.
    let error = args::parse(["status", "--nonsense"]).expect_err("an unknown flag must be refused");
    assert_eq!(
        error,
        UsageError::UnknownFlag {
            flag: "nonsense".to_owned(),
            known: args::FLAGS.iter().map(|flag| flag.long).collect(),
        }
    );
    let message = error.message();
    assert!(message.contains("--nonsense"), "{message}");
    assert!(message.contains("--budget"), "{message}");
}

#[test]
fn an_unknown_command_suggests_the_one_that_was_meant() {
    let error = args::parse(["contex"]).expect_err("a misspelled command must be refused");
    match &error {
        UsageError::UnknownCommand {
            name, suggestion, ..
        } => {
            assert_eq!(name, "contex");
            assert_eq!(
                *suggestion,
                Some("context"),
                "one edit away must be suggested"
            );
        }
        other => panic!("expected an unknown command, got {other:?}"),
    }
    assert!(
        error.message().contains("did you mean `context`"),
        "{error:?}"
    );
}

#[test]
fn a_command_nobody_nearly_typed_gets_no_suggestion() {
    // A wrong suggestion is worse than none, so the test pins that the matcher does not fire on
    // something entirely different.
    let error = args::parse(["zzzzzz"]).expect_err("an unknown command must be refused");
    match error {
        UsageError::UnknownCommand { suggestion, .. } => assert_eq!(suggestion, None),
        other => panic!("expected an unknown command, got {other:?}"),
    }
}

#[test]
fn a_flag_given_to_a_command_that_does_not_take_it_is_refused_rather_than_ignored() {
    // `peek status --budget 10` looks like it did something. It did not, and saying so is the
    // whole point of the per-command flag allow-list.
    let error = args::parse(["status", "--budget", "10"]).expect_err("must be refused");
    match &error {
        UsageError::FlagNotValidHere {
            flag,
            command,
            belongs_to,
        } => {
            assert_eq!(*flag, "budget");
            assert_eq!(*command, "status");
            assert_eq!(
                belongs_to,
                &vec!["context"],
                "--budget belongs to context only"
            );
        }
        other => panic!("expected a flag-not-valid-here error, got {other:?}"),
    }
}

#[test]
fn a_repeated_flag_is_refused_rather_than_resolved_by_taking_the_last() {
    // Last-wins is how a parameter degrades silently: the caller wrote two different budgets and
    // one of them was discarded without being named.
    let error = args::parse(["context", "Foo", "--budget", "10", "--budget", "20"])
        .expect_err("a repeated flag must be refused");
    assert_eq!(error, UsageError::RepeatedFlag { flag: "budget" });
    assert!(error.message().contains("more than once"), "{error:?}");
}

#[test]
fn a_value_that_is_not_a_number_is_refused_and_not_coerced() {
    // `--depth sideways` is exactly the audit-D shape. It must not become a depth.
    for value in ["sideways", "-1", "1.5", "", "0x10", "1e3"] {
        let error = args::parse(["dependents", "Foo", "--depth", value])
            .expect_err("a non-numeric depth must be refused");
        assert!(
            matches!(error, UsageError::NotANumber { flag: "depth", .. }),
            "--depth {value:?} produced {error:?}"
        );
    }
}

#[test]
fn a_switch_given_a_value_is_refused() {
    let error = args::parse(["status", "--json=yes"]).expect_err("must be refused");
    assert_eq!(error, UsageError::UnexpectedValue { flag: "json" });
}

#[test]
fn a_flag_that_needs_a_value_and_has_none_says_so() {
    let error = args::parse(["context", "Foo", "--budget"]).expect_err("must be refused");
    assert_eq!(error, UsageError::MissingValue { flag: "budget" });
    assert!(error.message().contains("ended"), "{error:?}");
}

#[test]
fn a_missing_target_is_refused_and_names_what_was_needed() {
    let error = args::parse(["explain"]).expect_err("explain needs a target");
    assert_eq!(
        error,
        UsageError::MissingArgument {
            command: "explain",
            expected: "<TARGET>",
        }
    );
}

#[test]
fn too_many_positional_arguments_are_refused_rather_than_dropped() {
    let error = args::parse(["status", "extra"]).expect_err("status takes no positionals");
    assert_eq!(
        error,
        UsageError::TooManyArguments {
            command: "status",
            accepts: 0,
        }
    );
}

#[test]
fn a_target_that_begins_with_a_dash_is_a_target_after_a_terminator() {
    // Otherwise there is no way to ask about anything whose name begins with a dash, and the only
    // options are a flag that does not exist or a quoting rule nobody can remember.
    let invocation = args::parse(["explain", "--", "--weird"]).expect("parse");
    match invocation.command {
        Command::Explain { target, .. } => assert_eq!(target, "--weird"),
        other => panic!("expected explain, got {other:?}"),
    }
}

#[test]
fn the_repository_defaults_to_the_working_directory_for_a_command_that_uses_a_flag() {
    let invocation = args::parse(["status"]).expect("parse");
    match invocation.command {
        Command::Status { root } => assert_eq!(root, Path::new(".")),
        other => panic!("expected status, got {other:?}"),
    }
}

#[test]
fn a_command_that_takes_a_positional_path_defaults_it_too() {
    // Both defaults are the same `.`, so `peek index` and `peek status` in one checkout cannot
    // disagree about which tree is meant.
    let invocation = args::parse(["index"]).expect("parse");
    match invocation.command {
        Command::Index { root, full } => {
            assert_eq!(root, Path::new("."));
            assert!(!full, "a rebuild is not the default; it is asked for");
        }
        other => panic!("expected index, got {other:?}"),
    }
}

#[test]
fn index_takes_its_repository_as_a_positional_and_the_others_take_root() {
    // Two sources for the same fact would be a precedence rule to get wrong. Each command has
    // exactly one.
    let invocation = args::parse(["index", "crates/peek-core"]).expect("parse");
    match invocation.command {
        Command::Index { root, .. } => assert_eq!(root, Path::new("crates/peek-core")),
        other => panic!("expected index, got {other:?}"),
    }
    let invocation = args::parse(["status", "--root", "crates/peek-core"]).expect("parse");
    match invocation.command {
        Command::Status { root } => assert_eq!(root, Path::new("crates/peek-core")),
        other => panic!("expected status, got {other:?}"),
    }
}

#[test]
fn the_full_flag_only_reaches_the_index_command() {
    let invocation = args::parse(["index", "--full"]).expect("parse");
    match invocation.command {
        Command::Index { full, .. } => assert!(full),
        other => panic!("expected index, got {other:?}"),
    }
    assert!(
        args::parse(["watch", "--full"]).is_err(),
        "`--full` belongs to index, and watch must not accept it silently"
    );
}

#[test]
fn no_arguments_prints_the_help_rather_than_failing() {
    // `peek` on its own is a request for orientation. Refusing it would be the opposite of
    // helpful, and it is a different thing from `peek <nonsense>`, which is refused below.
    assert_eq!(
        args::parse(Vec::<String>::new()).expect("parse").command,
        Command::Help
    );
    assert_eq!(args::parse(["help"]).expect("parse").command, Command::Help);
}

#[test]
fn the_help_and_version_flags_are_answers_and_are_read_wherever_they_appear() {
    // The flag table listed `-V, --version` and the parser never read it, so `peek --version` had
    // no positionals, fell through to the "no command at all" branch, and printed the help. The
    // help is generated from the table, so it described a flag that did nothing — the one thing
    // generating the documentation from the table is supposed to make impossible.
    //
    // Checked on both spellings and both positions, because either of them could have been the one
    // that kept working: a flag after the command is still the flag, so `peek status --version`
    // asks for the build rather than quietly running the status and dropping the version.
    for argv in [
        vec!["--version"],
        vec!["-V"],
        vec!["version"],
        vec!["status", "--version"],
        vec!["--version", "--json"],
    ] {
        assert_eq!(
            args::parse(argv.clone()).expect("parse").command,
            Command::Version,
            "{argv:?} did not reach the version command"
        );
    }
    for argv in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["status", "--help"],
        vec!["--help", "--json"],
    ] {
        assert_eq!(
            args::parse(argv.clone()).expect("parse").command,
            Command::Help,
            "{argv:?} did not reach the help command"
        );
    }
    // Neither of them takes a repository, so the fixture cannot be tricked into naming one for
    // them; `Command::root` answering `None` is what says that, and it is asserted here rather
    // than left to the fixture's own behaviour.
    assert!(
        args::parse(["--version"])
            .expect("parse")
            .command
            .root()
            .is_none()
    );
    assert!(
        args::parse(["--help"])
            .expect("parse")
            .command
            .root()
            .is_none()
    );
}

#[test]
fn the_depth_default_is_one_hop_because_that_is_what_one_hop_means() {
    // Not a threshold: one hop is the definition, and `callers` is `dependents` at depth 1.
    let invocation = args::parse(["dependents", "Foo"]).expect("parse");
    match invocation.command {
        Command::Dependents { depth, .. } => assert_eq!(depth, 1),
        other => panic!("expected dependents, got {other:?}"),
    }
}

#[test]
fn a_depth_of_zero_is_accepted_because_it_is_a_question_with_an_answer() {
    // D-0007: the target is never its own dependent, so a zero-depth walk is empty rather than
    // wrong. Refusing the argument would be worse than answering it.
    let invocation = args::parse(["dependents", "Foo", "--depth", "0"]).expect("parse");
    match invocation.command {
        Command::Dependents { depth, .. } => assert_eq!(depth, 0),
        other => panic!("expected dependents, got {other:?}"),
    }
}

#[test]
fn a_depth_beyond_the_hop_type_is_refused_rather_than_truncated() {
    let error = args::parse(["dependents", "Foo", "--depth", "99999999999"])
        .expect_err("a depth past u32 must be refused");
    assert!(
        matches!(error, UsageError::NotANumber { flag: "depth", .. }),
        "{error:?}"
    );
}

#[test]
fn the_budget_is_optional_at_the_parser_and_refused_by_the_command() {
    // The parser must not invent one, because a budget is a promise about what an answer costs.
    // The command refuses with the minimum, which is the only way a caller learns what to pass.
    let invocation = args::parse(["context", "Foo"]).expect("parse");
    match invocation.command {
        Command::Context { budget, target, .. } => {
            assert_eq!(budget, None, "no budget may be defaulted");
            assert_eq!(target, "Foo");
        }
        other => panic!("expected context, got {other:?}"),
    }
    let invocation = args::parse(["context", "Foo", "--budget", "0"]).expect("parse");
    match invocation.command {
        Command::Context { budget, .. } => assert_eq!(
            budget,
            Some(0),
            "an explicit zero is a budget of zero, not an absent one"
        ),
        other => panic!("expected context, got {other:?}"),
    }
}

#[test]
fn the_quiet_flag_is_a_boolean_on_every_command() {
    for argv in [
        vec!["status", "--quiet"],
        vec!["status", "-q"],
        vec!["index", "-q"],
        vec!["explain", "Foo", "--quiet"],
    ] {
        let invocation = args::parse(argv.clone()).expect("parse");
        assert!(invocation.quiet, "{argv:?} did not set --quiet");
    }
    assert!(!args::parse(["status"]).expect("parse").quiet);
}

#[test]
fn the_json_flag_is_carried_on_the_invocation_so_the_binary_need_not_reparse() {
    assert!(args::parse(["status", "--json"]).expect("parse").json);
    assert!(!args::parse(["status"]).expect("parse").json);
}

#[test]
fn the_index_dir_flag_is_a_path_and_is_not_turned_into_a_root() {
    let invocation = args::parse(["status", "--index-dir", "/tmp/peek-cache"]).expect("parse");
    assert_eq!(
        invocation.index_dir,
        Some(Path::new("/tmp/peek-cache").to_path_buf()),
        "the index location is a separate fact from the repository"
    );
}

#[test]
fn the_help_text_documents_every_flag_and_every_command_and_nothing_else() {
    // Generated from the tables, so this can only fail if a table row is unreachable or a flag is
    // documented twice. Both would be a documentation defect the user would act on.
    let text = args::help_text();
    for command in args::COMMANDS {
        assert!(
            text.contains(command.name),
            "the help omits the command `{}`",
            command.name
        );
        assert!(
            text.contains(command.summary),
            "the help omits the summary for `{}`",
            command.name
        );
    }
    for flag in args::FLAGS {
        assert!(
            text.contains(flag.long),
            "the help omits the flag `--{}`",
            flag.long
        );
        assert!(
            text.contains(flag.help),
            "the help omits the help text for `--{}`",
            flag.long
        );
    }
}

#[test]
fn the_help_text_states_the_exit_codes_and_what_each_one_means() {
    // An agent reads this. A code documented as a bare number is a code nobody can branch on.
    let text = args::help_text();
    for (code, meaning) in [
        (exit::EXIT_OK, "did what it was asked"),
        (exit::EXIT_FAILED, "could not do it"),
        (exit::EXIT_USAGE, "not understood"),
        (exit::EXIT_REFUSED, "refused"),
        (exit::EXIT_UNHEALTHY, "failing check"),
    ] {
        let line = format!("  {code}  ");
        assert!(
            text.contains(&line),
            "the help omits exit code {code}: {text}"
        );
        assert!(
            text.contains(meaning),
            "the help does not say what exit code {code} means"
        );
    }
}

#[test]
fn the_help_text_says_quiet_never_silences_a_finding() {
    let text = args::help_text();
    assert!(text.contains("findings never are"), "{text}");
}

#[test]
fn a_cluster_of_short_flags_is_refused_rather_than_half_applied() {
    // `clap` would accept `-qV`; this build does not, and says which flag it did not recognise
    // instead of applying the first and dropping the second.
    let error = args::parse(["-qV"]).expect_err("a cluster must be refused");
    assert!(matches!(error, UsageError::UnknownFlag { .. }), "{error:?}");
}

#[test]
fn the_watch_bounds_default_to_the_engine_quiet_period_times_twenty() {
    // Both numbers are stated as product decisions where they are defined, and the default is
    // derived rather than written down twice.
    let invocation = args::parse(["watch"]).expect("parse");
    match invocation.command {
        Command::Watch {
            quiet_for_ms,
            max_batch_ms,
            ..
        } => {
            assert_eq!(quiet_for_ms, args::DEFAULT_QUIET_FOR_MS);
            assert_eq!(
                max_batch_ms,
                args::DEFAULT_QUIET_FOR_MS * args::MAX_BATCH_QUIET_PERIODS
            );
        }
        other => panic!("expected watch, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The exit-code contract
// ---------------------------------------------------------------------------

#[test]
fn every_status_maps_to_exactly_one_exit_code_and_they_are_distinct() {
    // An agent branches on these numbers, so two statuses sharing a code is a caller that cannot
    // tell two situations apart — which is the defect the `refused` code exists to prevent.
    let codes = [
        Status::Ok.exit_code(),
        Status::Usage.exit_code(),
        Status::Refused.exit_code(),
        Status::Unhealthy.exit_code(),
        Status::Failed.exit_code(),
    ];
    let unique: std::collections::BTreeSet<u8> = codes.iter().copied().collect();
    assert_eq!(
        unique.len(),
        codes.len(),
        "two statuses share an exit code: {codes:?}"
    );
    assert_eq!(Status::Ok.exit_code(), 0, "success must be zero");
    for status in [
        Status::Usage,
        Status::Refused,
        Status::Unhealthy,
        Status::Failed,
    ] {
        assert_ne!(status.exit_code(), 0, "{status:?} must not exit zero");
    }
}

#[test]
fn a_usage_error_is_exit_two_and_a_refusal_is_exit_three() {
    // The two are different mistakes with different responses: one is fixed by retyping, the
    // other by asking a different question or raising a budget.
    let usage = exit::Failure::usage(
        "status",
        exit::Refusal::new(kind::UNKNOWN_FLAG, "unknown flag `--nope`"),
    );
    assert_eq!(usage.status, Status::Usage);
    assert_eq!(usage.exit_code(), 2);

    let refused = exit::Failure::refused(
        "context",
        exit::Refusal::new(kind::BUDGET_TOO_SMALL, "too small").with_minimum(60),
    );
    assert_eq!(refused.status, Status::Refused);
    assert_eq!(refused.exit_code(), 3);
}

#[test]
fn a_refusal_prints_its_candidates_and_its_minimum() {
    // D-0004: the caller disambiguates, so every candidate must travel with the refusal. D-0009:
    // a refused budget states the minimum, so one round trip is enough to fix it.
    let refusal = exit::Refusal::new(kind::AMBIGUOUS_TARGET, "three candidates")
        .with_candidates(vec![
            "src/a.rs::render".to_owned(),
            "src/b.rs::render".to_owned(),
        ])
        .with_minimum(60);
    let rendered = refusal.render();
    assert!(rendered.contains("2 candidate(s)"), "{rendered}");
    assert!(rendered.contains("src/a.rs::render"), "{rendered}");
    assert!(rendered.contains("src/b.rs::render"), "{rendered}");
    assert!(rendered.contains("60 token(s)"), "{rendered}");
}

#[test]
fn a_refusal_without_candidates_prints_no_candidate_section() {
    let refusal = exit::Refusal::new(kind::NO_INDEX, "no index");
    let rendered = refusal.render();
    assert!(!rendered.contains("candidate"), "{rendered}");
    assert!(!rendered.contains("smallest budget"), "{rendered}");
}

#[test]
fn a_failure_renders_the_command_and_the_reason_together() {
    let failure = exit::Failure::failed("status", exit::Refusal::new(kind::ENGINE, "disk full"));
    let rendered = failure.render();
    assert!(rendered.contains("status"), "{rendered}");
    assert!(rendered.contains("disk full"), "{rendered}");
}

// ---------------------------------------------------------------------------
// Quiet, and the two output modes
// ---------------------------------------------------------------------------

#[test]
fn quiet_suppresses_narration_and_changes_nothing_else() {
    // The strongest claim this crate makes about `--quiet`: the answer is byte-identical. If a
    // finding ever went through the narration sink this test would catch it.
    //
    // The comparison is on **`status`**, not on `index`, and that is the whole substance of the
    // fix. An earlier version ran `index` twice — once loud, once quiet — and asserted the two
    // renders matched. They cannot: the first is a build and the second a refresh, so the mode,
    // the generation and the removed counts all differ for reasons that have nothing to do with
    // `--quiet`. The test was failing on correct behaviour, and worse, it would have *passed* if
    // `--quiet` had been leaking a finding into the answer, because a leak of that size could
    // hide inside a difference the test blamed on the build.
    //
    // `status` reads and does not write, so the only thing that can differ between the two runs
    // is the flag.
    let repository = Repository::small("quiet");
    run(&repository, &["index", repository.root_str()]);

    let loud = run(&repository, &["status"]);
    let quiet = run(&repository, &["status", "--quiet"]);
    assert_eq!(
        loud.output.render(),
        quiet.output.render(),
        "--quiet silences the narration sink and nothing else"
    );
    assert_eq!(
        quiet.output.progress.len(),
        loud.output.progress.len(),
        "the recorded narration is complete in both modes; --quiet silences the sink, not the \
         record"
    );

    // And the narration that `index` does produce is still there, so the first run is not a no-op
    // and the sink is genuinely wired up.
    let narrated = run(&repository, &["index", repository.root_str()]);
    assert!(
        !narrated.output.progress.is_empty(),
        "index must narrate something"
    );
}

#[test]
fn every_command_records_its_narration_so_the_json_mode_is_complete() {
    // A JSON consumer that never receives the progress array cannot tell "nothing happened" from
    // "the caller asked for quiet".
    let repository = Repository::small("narration");
    let output = run(&repository, &["index", repository.root_str()]);
    assert!(
        output
            .output
            .progress
            .iter()
            .any(|line| line.contains("generation")),
        "no narration mentions the build: {:?}",
        output.output.progress
    );
}

#[test]
fn the_json_mode_is_the_serialisation_of_the_same_value_the_human_mode_formats() {
    // Not two renderings maintained side by side: the JSON is the value, and the human text is a
    // function of it. Round-tripping proves the value is complete and readable.
    let repository = Repository::small("json-round-trip");
    run(&repository, &["index", repository.root_str()]);
    let output = run(&repository, &["status", "--json"]);
    let text = output.output.render_json();
    let parsed: Output = serde_json::from_str(&text).expect("the JSON mode must parse back");
    assert_eq!(parsed, output.output, "the round trip must be lossless");
    assert_eq!(render(&output.output, &Mode::json()), text);
    assert_ne!(render(&output.output, &Mode::human()), text);
}

#[test]
fn the_json_mode_names_the_command_and_the_code_on_every_command() {
    // A consumer parsing this must not have to agree a schema out of band.
    let repository = Repository::small("json-envelope");
    run(&repository, &["index", repository.root_str()]);
    for (command, envelope, answer_tag, inner) in [
        (vec!["status"], "status", "status", ""),
        (vec!["doctor"], "doctor", "doctor", ""),
        (
            vec!["context", "wallet_charge", "--budget", "4000"],
            "context",
            "context",
            "",
        ),
        (vec!["explain", "wallet_charge"], "explain", "explain", ""),
        (
            vec!["callers", "wallet_charge"],
            "callers",
            "walk",
            "callers",
        ),
        (
            vec!["callees", "wallet_charge"],
            "callees",
            "walk",
            "callees",
        ),
        (
            vec!["dependents", "wallet_charge"],
            "dependents",
            "walk",
            "dependents",
        ),
    ] {
        let mut argv = command.clone();
        argv.push("--json");
        let output = run(&repository, &argv);
        let value: serde_json::Value =
            serde_json::from_str(&output.output.render_json()).expect("parse");
        assert_eq!(
            value["command"], envelope,
            "{command:?} named the wrong command"
        );
        assert_eq!(
            value["exit_code"], output.output.exit_code,
            "{command:?} disagreed with itself about the code"
        );
        assert_eq!(
            value["answer"]["command"], answer_tag,
            "{command:?} has an answer tagged with a different kind"
        );
        if !inner.is_empty() {
            assert_eq!(
                value["answer"]["command_name"], inner,
                "{command:?} must record which of the three names was used"
            );
        }
        assert!(
            value["version"].is_string(),
            "{command:?} does not say which build answered"
        );
    }
}

// ---------------------------------------------------------------------------
// `peek status`
// ---------------------------------------------------------------------------

#[test]
fn status_refuses_a_repository_with_no_index_rather_than_creating_one() {
    // `Store::open` creates a store when the file is absent, so a status that simply opened one
    // would answer "generation 0, nothing indexed" about a repository that was never indexed —
    // and a caller branching on a zero exit would read that as a measurement.
    let repository = Repository::empty("status-no-index");
    let outcome = run(&repository, &["status"]);
    assert_eq!(outcome.output.status, Status::Refused);
    assert_eq!(outcome.output.exit_code, 3);
    assert_eq!(
        outcome.output.refusal.as_ref().map(|r| r.kind.as_str()),
        Some(kind::NO_INDEX)
    );
    assert!(
        !repository.index_path().exists(),
        "the command must not have created the index it refused to report on"
    );
}

#[test]
fn status_reports_the_generation_the_store_actually_committed() {
    // Not zero, and not the count of files: the number a reader compares between runs to tell
    // "nothing changed" from "I am looking at a different index".
    let repository = Repository::small("status-generation");
    let built = run(&repository, &["index", repository.root_str()]);
    let built_generation = match &built.output.answer {
        Answer::Index(answer) => answer.generation,
        other => panic!("expected an index answer, got {other:?}"),
    };
    assert!(built_generation > 0, "a build must commit a generation");
    let status = run(&repository, &["status"]);
    match &status.output.answer {
        Answer::Status(StatusAnswer { generation, .. }) => assert_eq!(
            *generation, built_generation,
            "status must report the generation the last commit produced"
        ),
        other => panic!("expected a status answer, got {other:?}"),
    }
}

#[test]
fn status_reports_durability_read_back_from_the_connection_rather_than_assumed() {
    // `synchronous` is a per-connection setting, so a store opened without it has silently lost its
    // guarantee and the file itself would not say so.
    let repository = Repository::small("status-durability");
    run(&repository, &["index", repository.root_str()]);
    match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer { durability, .. }) => assert_eq!(
            durability, "FULL",
            "the default must be FULL; NORMAL would let a reported success be untrue on power loss"
        ),
        other => panic!("expected a status answer, got {other:?}"),
    }
}

#[test]
fn status_says_the_five_resolution_states_account_for_every_relation() {
    // A set of counts that does not partition cannot be checked by a reader, and this project
    // shipped exactly that once: counts that summed to less than the total they claimed, with
    // nothing in the output saying the remainder was simply not being reported.
    let repository = Repository::small("status-partition");
    run(&repository, &["index", repository.root_str()]);
    match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer {
            counts,
            states_partition,
            ..
        }) => {
            assert!(states_partition, "the counts do not partition: {counts:?}");
            let sum = counts.resolved_relations
                + counts.ambiguous_relations
                + counts.unresolved_relations
                + counts.inferred_relations
                + counts.pending_relations;
            assert_eq!(sum, counts.relation_count, "{counts:?}");
        }
        other => panic!("expected a status answer, got {other:?}"),
    }
}

#[test]
fn status_states_what_it_did_not_do() {
    // A status that does not say what it did not measure is a confident wrong answer.
    let repository = Repository::small("status-did-not");
    run(&repository, &["index", repository.root_str()]);
    let answer = run(&repository, &["status"]).output.answer;
    let did_not = answer.did_not();
    assert!(!did_not.is_empty(), "status must state its limits");
    let joined = did_not.join(" ");
    for expected in [
        "did not walk the repository",
        "integrity check",
        "did not compare the index against the working tree",
        "did not read a byte of source",
        // The two file sizes it does print can count the same page twice, so it has to say that
        // it did not divide them: a status report that prints the pair and stays silent about it
        // is the confident wrong answer this list exists to prevent.
        "did not compare the two file sizes",
    ] {
        assert!(
            joined.contains(expected),
            "status does not say: {expected}\n{joined}"
        );
    }
}

// ---------------------------------------------------------------------------
// The counts the two commands share
// ---------------------------------------------------------------------------

#[test]
fn the_counts_type_is_one_definition_used_by_both_status_and_doctor() {
    // A number that means different things in two commands is worse than a missing one. `Counts`
    // is constructed from `StoreStats` once, and both commands carry it.
    let repository = Repository::small("counts-shared");
    run(&repository, &["index", repository.root_str()]);
    let from_status = match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => *counts,
        other => panic!("expected a status answer, got {other:?}"),
    };
    let from_doctor = match &run(&repository, &["doctor"]).output.answer {
        Answer::Doctor(answer) => answer.counts.expect("doctor must report the counts"),
        other => panic!("expected a doctor answer, got {other:?}"),
    };
    assert_eq!(
        from_status, from_doctor,
        "status and doctor disagree about a number; they must be the same measurement"
    );
}

#[test]
fn a_counts_value_reports_whether_its_states_partition() {
    let mut counts = Counts {
        generation: 3,
        schema_version: 2,
        entity_count: 10,
        relation_count: 6,
        resolved_relations: 2,
        ambiguous_relations: 1,
        unresolved_relations: 1,
        inferred_relations: 1,
        pending_relations: 1,
        candidate_count: 2,
        orphan_relations: 0,
        file_size_bytes: 4096,
        wal_size_bytes: 0,
    };
    assert!(counts.states_partition());
    counts.pending_relations = 2;
    assert!(
        !counts.states_partition(),
        "adding an unaccounted relation must be detected, not absorbed"
    );
}

// ---------------------------------------------------------------------------
// `peek index`
// ---------------------------------------------------------------------------

#[test]
fn a_cold_build_reports_the_mode_and_the_files_it_indexed() {
    // **The fixture changed, not the assertion.** This used to build a `Repository::empty` and
    // then assert `files_indexed > 0`, which cannot hold: there is no file in an empty directory
    // to index, so the test was asserting something untrue for the tree it had made. Weakening it
    // to `files_indexed == 0` would have been worse than useless — `a_run_that_indexed_nothing_
    // still_exits_zero_but_says_the_run_was_empty` already asserts that, and the word "cold" is
    // about the *index*, not the tree. A repository with four files in it and no generation
    // committed is exactly as cold as an empty one, so the fixture is the thing that was wrong.
    let repository = Repository::small("index-cold");
    let outcome = run(&repository, &["index", repository.root_str()]);
    assert_eq!(outcome.output.status, Status::Ok);
    assert_eq!(outcome.output.exit_code, 0);
    match &outcome.output.answer {
        Answer::Index(answer) => {
            assert_eq!(
                answer.mode, "build",
                "no generation exists, so this is a cold build"
            );
            assert!(answer.files_indexed > 0, "nothing was indexed: {answer:?}");
            assert!(answer.generation > 0);
            assert!(
                answer.resolution.is_some(),
                "a build without a resolution pass leaves every relation pending"
            );
        }
        other => panic!("expected an index answer, got {other:?}"),
    }
}

#[test]
fn a_warm_run_refreshes_rather_than_rebuilding() {
    let repository = Repository::small("index-warm");
    let first = run(&repository, &["index", repository.root_str()]);
    let second = run(&repository, &["index", repository.root_str()]);
    let (first_answer, second_answer) = match (&first.output.answer, &second.output.answer) {
        (Answer::Index(first), Answer::Index(second)) => (first.clone(), second.clone()),
        other => panic!("expected two index answers, got {other:?}"),
    };
    assert_eq!(first_answer.mode, "build");
    assert_eq!(
        second_answer.mode, "refresh",
        "a second run must not rebuild from scratch"
    );
    assert!(
        second_answer.generation > first_answer.generation,
        "a refresh commits, so the generation must advance: {} then {}",
        first_answer.generation,
        second_answer.generation
    );
}

#[test]
fn a_full_rebuild_clears_the_index_and_names_what_it_cleared() {
    // `--full` is a real rebuild, not a flag that only changes a label: the top-level entries the
    // index held are removed in one transaction before the build, and the answer names them.
    let repository = Repository::small("index-full");
    run(&repository, &["index", repository.root_str()]);
    let rebuilt = run(&repository, &["index", repository.root_str(), "--full"]);
    match &rebuilt.output.answer {
        Answer::Index(answer) => {
            assert_eq!(answer.mode, "rebuild");
            assert!(
                !answer.cleared_paths.is_empty(),
                "a rebuild must name the top-level entries it cleared: {answer:?}"
            );
            assert!(
                answer
                    .did_not
                    .iter()
                    .any(|line| line.contains("crash between them")),
                "a clear and a build are separate transactions, and that must be said: {:?}",
                answer.did_not
            );
        }
        other => panic!("expected an index answer, got {other:?}"),
    }
}

#[test]
fn a_full_rebuild_on_an_empty_index_says_there_was_nothing_to_clear() {
    // A zero here is a measurement, and a command that reported "cleared 0" without saying so
    // would leave the reader unable to distinguish it from having cleared nothing on purpose.
    let repository = Repository::empty("index-full-empty");
    let rebuilt = run(&repository, &["index", repository.root_str(), "--full"]);
    match &rebuilt.output.answer {
        Answer::Index(answer) => {
            assert!(answer.cleared_paths.is_empty());
            assert!(
                answer
                    .notes
                    .iter()
                    .any(|note| note.contains("nothing to clear")),
                "{:?}",
                answer.notes
            );
        }
        other => panic!("expected an index answer, got {other:?}"),
    }
}

#[test]
fn a_deleted_file_is_removed_from_the_index_by_a_later_run() {
    // A file that was deleted is not in the discovery walk, so a refresh of what the walk yields
    // would leave it indexed forever: a stale index still answering questions about code that is
    // gone. This is the test that says the command noticed.
    let repository = Repository::small("index-deleted");
    run(&repository, &["index", repository.root_str()]);
    let before = match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => counts.entity_count,
        other => panic!("expected a status answer, got {other:?}"),
    };
    repository.remove("src/ledger.rs");
    let after_run = run(&repository, &["index", repository.root_str()]);
    match &after_run.output.answer {
        Answer::Index(answer) => assert_eq!(
            answer.stale_paths_removed, 1,
            "exactly one indexed file is no longer on disk: {answer:?}"
        ),
        other => panic!("expected an index answer, got {other:?}"),
    }
    let after = match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => counts.entity_count,
        other => panic!("expected a status answer, got {other:?}"),
    };
    assert!(
        after < before,
        "the removed file's entities must be gone: {before} then {after}"
    );
}

#[test]
fn a_file_that_is_not_valid_utf8_is_refused_rather_than_partially_indexed() {
    // One bad file must degrade that file and not abort the repository — the exact failure this
    // engine replaces, where a single non-UTF-8 file failed the whole run and persisted nothing.
    // And the refusal must be counted *and named*, because a run that quietly indexed three of four
    // files is the dishonesty this project exists to remove.
    let repository = Repository::small("index-not-utf8");
    repository.write_invalid_utf8("src/broken.rs");
    let outcome = run(&repository, &["index", repository.root_str()]);
    assert_eq!(
        outcome.output.exit_code, 0,
        "one bad file must not fail the run"
    );
    match &outcome.output.answer {
        Answer::Index(answer) => {
            assert!(
                answer.files_skipped > 0,
                "the undecodable file must be counted as skipped: {answer:?}"
            );
            assert!(
                answer
                    .skipped
                    .iter()
                    .any(|entry| entry.path.contains("broken.rs")),
                "the undecodable file must be named: {:?}",
                answer.skipped
            );
            assert!(
                answer.files_indexed > 0,
                "the readable files must still have been indexed: {answer:?}"
            );
        }
        other => panic!("expected an index answer, got {other:?}"),
    }
    // And the run is still consistent afterwards.
    match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => assert!(
            counts.states_partition(),
            "a run that degraded one file must still leave a consistent index: {counts:?}"
        ),
        other => panic!("expected a status answer, got {other:?}"),
    }
}

#[test]
fn a_file_the_build_could_not_index_is_counted_and_named() {
    // Discovery *counts* what it refused and, for the cases worth naming, says which file. The
    // report must carry both: the count tells a reader the shape, the path makes it actionable.
    let repository = Repository::empty("index-skipped");
    repository.write("README.md", "# not a language this build extracts\n");
    let outcome = run(&repository, &["index", repository.root_str()]);
    match &outcome.output.answer {
        Answer::Index(answer) => {
            assert!(
                answer.files_unsupported > 0,
                "a file with no extraction rules must be counted, not indexed: {answer:?}"
            );
            assert!(
                answer.skipped_by_reason.values().sum::<u64>() > 0,
                "the refusals must be grouped by reason: {:?}",
                answer.skipped_by_reason
            );
            assert!(
                answer
                    .did_not
                    .iter()
                    .any(|line| line.contains("no registered extraction rules")),
                "a language with no rules is not the same as a file with no symbols, and must be \
                 said"
            );
        }
        other => panic!("expected an index answer, got {other:?}"),
    }
}

#[test]
fn a_run_that_indexed_nothing_still_exits_zero_but_says_the_run_was_empty() {
    // Not a failure: an empty directory is a legitimate repository. But a reader must not have to
    // infer emptiness from a zero.
    let repository = Repository::empty("index-nothing");
    let outcome = run(&repository, &["index", repository.root_str()]);
    assert_eq!(outcome.output.exit_code, 0);
    match &outcome.output.answer {
        Answer::Index(answer) => {
            assert_eq!(answer.files_indexed, 0);
            assert!(
                answer.resolution_states_partition,
                "an empty run must still partition"
            );
        }
        other => panic!("expected an index answer, got {other:?}"),
    }
    // And `doctor` is where an empty index is diagnosed.
    let doctor = run(&repository, &["doctor"]);
    match &doctor.output.answer {
        Answer::Doctor(answer) => assert!(
            answer
                .findings
                .iter()
                .any(|f| f.check == "generation" && f.summary.contains("empty")),
            "doctor must name the empty index: {answer:?}"
        ),
        other => panic!("expected a doctor answer, got {other:?}"),
    }
}

#[test]
fn a_run_over_a_path_that_is_not_a_directory_is_a_usage_error() {
    // **Exempt from the harness's root injection, and deliberately so.** The claim is about the
    // *argument*: naming a file where a repository root belongs must be refused as
    // `not_a_repository`. Injecting the fixture's root as `index`'s `[PATH]` positional would
    // give the command two positionals, and the refusal would be `wrong_arity` instead — a
    // different refusal that this test would still have been happy to assert. It is run through
    // `run_verbatim` so the exemption is visible here rather than hidden in a list inside the
    // fixture.
    let repository = Repository::small("index-not-a-dir");
    let file = repository.root_str().to_owned() + "/src/ledger.rs";
    let outcome = run_verbatim(&repository, &["index", &file]);
    assert_eq!(outcome.output.status, Status::Usage);
    assert_eq!(outcome.output.exit_code, 2);
    assert_eq!(
        outcome.output.refusal.as_ref().map(|r| r.kind.as_str()),
        Some(kind::NOT_A_REPOSITORY)
    );
}

// ---------------------------------------------------------------------------
// `peek rm`
// ---------------------------------------------------------------------------

#[test]
fn rm_removes_a_files_rows_and_leaves_the_file_on_disk() {
    // The command is named `rm` and it removes *rows*. No file is unlinked, and saying so is not
    // politeness: a command called `rm` that appears to delete source is run with wrong
    // expectations, and the cost of finding out is somebody's work.
    let repository = Repository::small("rm-file");
    run(&repository, &["index", repository.root_str()]);
    let before = match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => counts.relation_count,
        other => panic!("expected a status answer, got {other:?}"),
    };
    let outcome = run(
        &repository,
        &["rm", "src/ledger.rs", "--root", repository.root_str()],
    );
    assert_eq!(outcome.output.exit_code, 0);
    match &outcome.output.answer {
        Answer::Remove(answer) => {
            assert_eq!(answer.scope, "file");
            assert_eq!(
                answer.indexed_paths_removed, 1,
                "one file was indexed at that path"
            );
            assert!(
                answer.entities_removed > 0,
                "nothing was deleted: {answer:?}"
            );
            assert!(
                answer
                    .notes
                    .iter()
                    .any(|note| note.contains("no_candidate")),
                "edges that pointed in are demoted, and that must be said: {:?}",
                answer.notes
            );
            assert!(
                answer
                    .did_not
                    .iter()
                    .any(|line| line.contains("did not touch the filesystem")),
                "{:?}",
                answer.did_not
            );
        }
        other => panic!("expected a remove answer, got {other:?}"),
    }
    assert!(
        repository.root().join("src/ledger.rs").is_file(),
        "the file must still be on disk"
    );
    let after = match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => counts.relation_count,
        other => panic!("expected a status answer, got {other:?}"),
    };
    assert!(
        after < before,
        "relations must be gone: {before} then {after}"
    );
}

#[test]
fn rm_on_a_subtree_removes_everything_at_or_below_it_and_says_so() {
    let repository = Repository::small("rm-subtree");
    run(&repository, &["index", repository.root_str()]);
    // **`src`, and not `src/ledger`.** The fixture's directory is `src`; the only thing it holds
    // that is called `ledger` is the *file* `src/ledger.rs`, so `src/ledger` names nothing at all.
    // `RemoveAnswer::scope` is "decided by whether a directory is there now" and the command
    // documents the same rule ("A path that names a directory removes the subtree; anything else
    // removes exactly that file"), so a path with no directory under it is correctly a `file` scope
    // covering zero indexed rows. The earlier spelling of this test asserted `subtree` for a path
    // its own fixture did not contain, and was failing on correct behaviour. Naming the real
    // directory keeps every assertion below exactly as strong.
    let outcome = run(&repository, &["rm", "src", "--root", repository.root_str()]);
    match &outcome.output.answer {
        Answer::Remove(answer) => {
            assert_eq!(answer.scope, "subtree");
            assert!(
                answer.indexed_paths_removed > 0,
                "nothing was covered: {answer:?}"
            );
            assert!(
                answer
                    .did_not
                    .iter()
                    .any(|line| line.contains("whole subtree")),
                "the scope must be stated, because `rm src` and `rm src/main.rs` are different \
                 claims"
            );
        }
        other => panic!("expected a remove answer, got {other:?}"),
    }
}

#[test]
fn rm_on_a_path_the_index_never_held_reports_a_measured_zero() {
    // A zero and an unperformed check are different facts, and a reader cannot tell them apart
    // from a bare `0`.
    let repository = Repository::small("rm-absent");
    run(&repository, &["index", repository.root_str()]);
    let outcome = run(
        &repository,
        &["rm", "src/never_seen.rs", "--root", repository.root_str()],
    );
    assert_eq!(
        outcome.output.exit_code, 0,
        "a path the index never held is a measured zero, not a usage error: {:?}",
        outcome.output.refusal
    );
    match &outcome.output.answer {
        Answer::Remove(answer) => {
            // Both assertions carry the whole answer, so a failure says which field was wrong
            // *and* what the removal actually scoped, did and claimed — which is the difference
            // between a number that is off and a number that means something has gone wrong.
            assert_eq!(answer.indexed_paths_removed, 0, "{answer:?}");
            assert_eq!(answer.entities_removed, 0, "{answer:?}");
            assert!(
                answer.notes.iter().any(|note| note.contains("measurement")),
                "the zero must be labelled a measurement: {:?}",
                answer.notes
            );
        }
        other => panic!("expected a remove answer, got {other:?}"),
    }
}

#[test]
fn rm_refuses_a_path_outside_the_repository_and_names_both_places() {
    // `/x/repo-old` is not inside `/x/repo`. A prefix comparison would say it is, and that is the
    // mistake that lets one project's index be removed by a command aimed at another.
    let repository = Repository::small("rm-outside");
    run(&repository, &["index", repository.root_str()]);
    let outside = repository.sibling("elsewhere").join("secret.rs");
    let outcome = run(&repository, &["rm", outside.to_str().expect("utf-8")]);
    assert_eq!(outcome.output.exit_code, 2);
    let refusal = outcome.output.refusal.expect("a refusal is required");
    assert_eq!(refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
    assert!(
        refusal.message.contains(repository.root_str()),
        "{}",
        refusal.message
    );
    assert!(refusal.message.contains("elsewhere"), "{}", refusal.message);
}

#[test]
fn rm_on_a_deleted_file_still_works_because_that_is_when_it_is_needed() {
    // The file is gone from disk and still in the index: the case a deletion creates. A command
    // that refused here would be useless exactly when it is useful.
    let repository = Repository::small("rm-deleted");
    run(&repository, &["index", repository.root_str()]);
    repository.remove("src/ledger.rs");
    let outcome = run(
        &repository,
        &["rm", "src/ledger.rs", "--root", repository.root_str()],
    );
    assert_eq!(outcome.output.exit_code, 0);
    match &outcome.output.answer {
        Answer::Remove(answer) => assert_eq!(
            answer.indexed_paths_removed, 1,
            "the index still held the deleted file and must have removed it: {answer:?}"
        ),
        other => panic!("expected a remove answer, got {other:?}"),
    }
}

#[test]
fn rm_refuses_a_repository_with_no_index_rather_than_creating_one() {
    let repository = Repository::empty("rm-no-index");
    let outcome = run(
        &repository,
        &["rm", "src/a.rs", "--root", repository.root_str()],
    );
    assert_eq!(outcome.output.exit_code, 3);
    assert_eq!(
        outcome.output.refusal.as_ref().map(|r| r.kind.as_str()),
        Some(kind::NO_INDEX)
    );
    assert!(!repository.index_path().exists());
}

#[test]
fn the_test_harness_gives_each_repository_its_own_index_and_cleans_up() {
    // Every test above relies on this: two repositories sharing an index would make each of them
    // assert on the other's counts. The harness failing here explains every one of them at once.
    //
    // **This test used to hang the whole binary.** Each `Repository` held the process-global
    // `Mutex` around `set_root_override` for its whole lifetime, and `std::sync::Mutex` is not
    // reentrant, so constructing the second repository blocked on a guard the first still held —
    // forever, with no output, which reads as a run that produced no results rather than as a
    // defect. The override is now installed for the length of a single run instead, which is why
    // two repositories can be alive at once and why the run below is pointed at the right one.
    let first = Repository::small("harness-a");
    let second = Repository::small("harness-b");
    assert_ne!(
        first.index_path(),
        second.index_path(),
        "two repositories must not share an index"
    );
    let first_path = first.index_path();
    let second_path = second.index_path();
    run(&first, &["index"]);
    run(&second, &["index"]);
    assert!(
        first_path.is_file(),
        "the build must have created the index at {first_path:?}"
    );
    assert!(
        second_path.is_file(),
        "the second build must have created its own index at {second_path:?}, not the first's"
    );
    let _cleanup = Cleanup::on(first_path.clone());
    drop(first);
    assert!(
        !first_path.exists(),
        "dropping the repository must have removed its index"
    );
    assert!(
        second_path.is_file(),
        "dropping one repository must not remove another's index"
    );
}

#[test]
fn the_fixture_names_the_repository_for_a_command_that_takes_a_flag() {
    // `--root` defaults to `.` and a test binary's working directory is the *package* directory,
    // so `run(&repository, &["status"])` used to answer about `crates/peek-cli`. It refused with
    // `no_index`, which reads as a wrong answer type rather than as the missing flag it was, and
    // fourteen tests were asserting real behaviour about a directory that is not a fixture.
    //
    // Asserted through what the command *did*, not through the command line: `status` takes no
    // positional, so a fixture that named the repository as one would be refused for arity, and
    // this test would fail. Reaching the fixture's own index is therefore the proof that the flag
    // was used.
    let repository = Repository::small("harness-flag-root");
    let built = run(&repository, &["index"]);
    assert_eq!(built.output.exit_code, 0, "{:?}", built.output.refusal);
    let status = run(&repository, &["status"]);
    assert_eq!(
        status.output.status,
        Status::Ok,
        "the command must have found the fixture's index: {:?}",
        status.output.refusal
    );
    assert_eq!(
        status.output.root,
        repository.root_str(),
        "the answer names the tree it read"
    );
}

#[test]
fn the_fixture_names_the_repository_for_a_command_that_takes_a_positional() {
    // The same claim for the other shape. `index` reads its repository from the `[PATH]`
    // positional, and `--root` is a global flag that `index` **accepts and ignores** — so a
    // fixture that used the flag for every command would index the package directory and report a
    // confident count for the wrong tree. That is worse than the bug it fixes, which is why the
    // shape is read out of the command table rather than decided here.
    //
    // Asserted by leaving the positional out entirely: if the harness injected the flag instead,
    // the build would walk the working directory, index nothing, and produce no index file here.
    let repository = Repository::small("harness-positional-root");
    let built = run(&repository, &["index"]);
    assert_eq!(built.output.exit_code, 0, "{:?}", built.output.refusal);
    assert!(
        repository.index_path().is_file(),
        "the build must have written the index under the fixture, not the package directory"
    );
    match &built.output.answer {
        Answer::Index(answer) => assert!(
            answer.files_indexed > 0,
            "the fixture's own four files must have been indexed: {answer:?}"
        ),
        other => panic!("expected an index answer, got {other:?}"),
    }
}

#[test]
fn the_fixture_leaves_a_command_line_that_already_names_a_repository_alone() {
    // Adding a second `--root` would be a `RepeatedFlag` usage error, so a test that spells its
    // own root — a canonicalisation test naming `<root>/src/..`, for instance — would stop testing
    // canonicalisation and start testing the harness. "Already named" is decided from the parsed
    // command rather than by counting tokens, so a new spelling of the flag does not defeat it.
    let repository = Repository::small("harness-already-named");
    run(&repository, &["index", repository.root_str()]);
    let roundabout = format!("{}/src/..", repository.root_str());
    let status = run(&repository, &["status", "--root", &roundabout]);
    assert_eq!(
        status.output.status,
        Status::Ok,
        "{:?}",
        status.output.refusal
    );
    let spelled_out = run(&repository, &["status", &format!("--root={roundabout}")]);
    assert_eq!(
        spelled_out.output.status,
        Status::Ok,
        "{:?}",
        spelled_out.output.refusal
    );
    // And the two spellings reached one index, which is the property the roundabout spelling
    // exists to demonstrate and the reason the harness must not rewrite it.
    assert_eq!(status.output.index_path, spelled_out.output.index_path);
}

#[test]
#[should_panic(expected = "the harness cannot tell which directory")]
fn a_root_the_filesystem_will_not_locate_is_not_called_a_misdirected_command() {
    // **A claim the check used to make without having established it.** The check could not
    // locate a named root, and it answered by falling back to the spelling, comparing that with the
    // fixture's canonical root, and then reporting that the command had not been pointed at the
    // fixture. All it had observed was that it could not ask — and being unable to look something
    // up is a fact about existence, which was being reported as a fact about location.
    //
    // Two different roots reach that state: one that names nothing at all, and one that names the
    // fixture through a spelling the filesystem will not answer about whole. The message named the
    // first, so the second reported a harness failure for a command pointed exactly where it was
    // asked to point — which is how a test suite that was never wrong looked broken.
    //
    // **This fails on the old check and passes on this one, and it does so on every platform**,
    // because the difference between the two is entirely what the harness is entitled to claim, and
    // a claim is observable through the message. Asking about the misdirection instead of about the
    // uncertainty is the assertion; the filesystem could not have told them apart either way.
    let repository = Repository::small("harness-unlocated-root");
    let nowhere = repository.root().join("never-written");

    // The fixture's own precondition, asserted rather than assumed: a root the filesystem *can*
    // locate is not the case under test, and a test that quietly became one would assert nothing.
    assert!(
        nowhere.canonicalize().is_err(),
        "{} has to name nothing at all, or this is about a root the filesystem will locate and \
         the message under test is never the one that is produced",
        nowhere.display()
    );

    run(
        &repository,
        &["status", "--root", nowhere.to_str().expect("utf-8")],
    );
}

#[test]
fn one_directory_named_through_a_climb_is_still_the_fixtures_root() {
    // **Both spellings are derived from one canonical path, and that is the whole point.** The
    // obvious way to build this pair is to take the machine's temporary directory and canonicalise
    // it, which compares a path with itself wherever the two agree — and on a kernel they always
    // agree, because `canonicalize` is `realpath` and `realpath` takes the climb off for you. A
    // test written that way passes vacuously on the only platform it can be measured on, which is
    // how the equivalent defect in the library's own resolution survived. This one builds the
    // second spelling out of the first, so they differ by construction rather than by whatever the
    // runner's temporary directory happens to look like.
    let repository = Repository::small("harness-climbed-root");
    let roundabout = repository.root().join("src").join("..");

    // The spellings differ as paths, asserted rather than assumed, because that is the property
    // the construction is for and it is the thing a hand-written fixture gets wrong.
    assert_ne!(
        roundabout,
        repository.root().to_path_buf(),
        "the two spellings have to differ as paths, or the check is handed the same string twice \
         and cannot tell a directory from its own spelling"
    );
    // And they differ by exactly one climb over one directory the fixture really has. A climb over
    // a name that does not exist names nothing at all, which is the other test.
    assert!(
        repository.root().join("src").is_dir(),
        "the climb is spent on a directory that exists; over one that does not, the spelling names \
         nothing and the check is right to refuse it"
    );
    // The spelling ends in the climb. `Path::file_name` answers `None` for a trailing `..`,
    // because `..` is not a normal component — so "ends in the climb" is asserted as the
    // string ending, and `file_name() == None` is the *consequence* worth pinning too, since
    // that is the fact the resolution path actually has to cope with.
    assert_eq!(
        roundabout.file_name(),
        None,
        "a trailing `..` is not a file name, and code that assumed otherwise is the bug this test \
         exists beside"
    );
    assert!(
        roundabout.to_string_lossy().ends_with(".."),
        "the spelling has to end in the climb, or the round trip is not one: {roundabout:?}"
    );
    assert_eq!(
        roundabout.parent().and_then(|inside| inside.parent()),
        Some(repository.root()),
        "and the rest of it has to be the fixture's root reached through one directory, or the \
         climb is being spent on the wrong name"
    );

    // **The check, on the case this fixture exists for.** Where the filesystem answers about the
    // whole spelling at once — every kernel, and Windows for a spelling with no climb on it — this
    // is `realpath` and both sides are one location. Where it will not answer, the check has to
    // take the climb off itself, and **this test cannot reach that branch from a kernel**: there
    // `canonicalize` never declines for a spelling that names a directory which exists, so there
    // is nothing on this platform that reaches it. That branch exists for the platform whose path
    // parser hands a verbatim spelling straight to the filesystem with the climb still written
    // down, and the assertion above is what holds on every platform.
    run(
        &repository,
        &["status", "--root", roundabout.to_str().expect("utf-8")],
    );
}

#[test]
#[should_panic(expected = "the fixture did not point this command at its own repository")]
fn a_directory_inside_the_fixture_is_not_the_fixtures_root() {
    // **The other half of the contract, and the half a fix is most likely to break.** Reconciling
    // two spellings of one directory is easy to do by comparing *less*: a containment test or a
    // component-prefix test accepts `<root>/src` and every other subdirectory, and then the check
    // no longer catches the one thing it exists to catch. So the directory is inside the fixture —
    // it exists, the filesystem locates it without any trouble at all, and it is not the root.
    let repository = Repository::small("harness-not-the-root");
    let inside = repository.root().join("src");
    assert!(
        inside.is_dir(),
        "the fixture has to contain a directory that is not its root, or this is about a path \
         nothing can locate and the other test already covers it: {}",
        inside.display()
    );
    run(
        &repository,
        &["status", "--root", inside.to_str().expect("utf-8")],
    );
}

#[test]
fn a_command_line_the_parser_refuses_is_never_given_a_root() {
    // The four cases in `failure.rs` exist to assert on the refusal, and appending `--root` to
    // `--nonsense` would turn an `unknown_flag` into a `wrong_arity` — a test still passing, now
    // checking nothing. The exemption is a property of a line that does not parse at all, so it
    // applies to every one of them without any of them having to know.
    let repository = Repository::small("harness-unparsable");
    for (argv, expected) in [
        (vec!["status", "--nonsense"], kind::UNKNOWN_FLAG),
        (vec!["contex"], kind::UNKNOWN_COMMAND),
        (vec!["context", "--budget"], kind::MISSING_VALUE),
        (vec!["status", "extra"], kind::WRONG_ARITY),
    ] {
        let output = run(&repository, &argv).output;
        assert_eq!(output.status, Status::Usage, "{argv:?}");
        assert_eq!(
            output.refusal.as_ref().map(|r| r.kind.as_str()),
            Some(expected),
            "{argv:?}: the harness changed which refusal this is"
        );
    }
}

#[test]
fn the_exempt_command_lines_still_mean_what_they_meant() {
    // The three tests that are not allowed to have a repository injected, checked together so
    // that adding a fourth one is a deliberate act rather than an accident. Each is named with
    // what it is exempt from and why; the point of this test is that the exemptions still hold.
    let repository = Repository::small("exemptions");

    // 1. `a_run_over_a_path_that_is_not_a_directory_is_a_usage_error` — the argument is the file,
    //    so a second positional would change the refusal from `not_a_repository` to `wrong_arity`.
    let file = repository.root_str().to_owned() + "/src/ledger.rs";
    let not_a_directory = run_verbatim(&repository, &["index", &file]).output;
    assert_eq!(
        not_a_directory.refusal.as_ref().map(|r| r.kind.as_str()),
        Some(kind::NOT_A_REPOSITORY),
        "the refusal must be about the path, not about how many there were"
    );

    // 2. `rm_refuses_a_path_outside_the_repository_and_names_both_places` — this one is *not*
    //    exempt; it is the test that catches the harness forgetting. It names no root at all, and
    //    its whole claim is that the refusal names the repository the command was pointed at.
    //    Under the old harness that was the package directory, so the message did not contain the
    //    fixture and the test failed for a reason nobody could see.
    run(&repository, &["index", repository.root_str()]);
    let outside = repository.sibling("elsewhere").join("secret.rs");
    let named = outside.to_str().expect("utf-8");
    let escaped = run(&repository, &["rm", named]).output;
    assert_eq!(escaped.exit_code, 2);
    let refusal = escaped.refusal.expect("a refusal is required");
    assert_eq!(refusal.kind.as_str(), kind::OUTSIDE_REPOSITORY);
    assert!(
        refusal.message.contains(repository.root_str()),
        "the refusal must name the repository the command was actually pointed at: {}",
        refusal.message
    );

    // 3. `a_command_that_worked_never_carries_a_declined_answer` — `help` and `version` read no
    //    repository, so naming one would be a lie about what they read, and the parser ignores it
    //    anyway, which makes it a dead flag. A command that reports having read a tree it never
    //    opened is the defect this project exists to remove.
    for argv in [vec!["--help"], vec!["--version"]] {
        let output = run(&repository, &argv).output;
        assert_eq!(output.exit_code, 0, "{argv:?}");
        assert!(
            output.root.is_empty() && output.index_path.is_empty(),
            "{argv:?} must not claim to have read a repository: {output:?}"
        );
    }
}
