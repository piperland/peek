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

use fixture::{Cleanup, Repository, run};

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
        UsageError::UnknownCommand { name, suggestion, .. } => {
            assert_eq!(name, "contex");
            assert_eq!(*suggestion, Some("context"), "one edit away must be suggested");
        }
        other => panic!("expected an unknown command, got {other:?}"),
    }
    assert!(error.message().contains("did you mean `context`"), "{error:?}");
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
            assert_eq!(belongs_to, &vec!["context"], "--budget belongs to context only");
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
    assert_eq!(args::parse(Vec::<String>::new()).expect("parse").command, Command::Help);
    assert_eq!(args::parse(["help"]).expect("parse").command, Command::Help);
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
    assert!(matches!(error, UsageError::NotANumber { flag: "depth", .. }), "{error:?}");
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
        assert!(text.contains(&line), "the help omits exit code {code}: {text}");
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
    assert_eq!(unique.len(), codes.len(), "two statuses share an exit code: {codes:?}");
    assert_eq!(Status::Ok.exit_code(), 0, "success must be zero");
    for status in [Status::Usage, Status::Refused, Status::Unhealthy, Status::Failed] {
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
        .with_candidates(vec!["src/a.rs::render".to_owned(), "src/b.rs::render".to_owned()])
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
    let repository = Repository::small("quiet");
    let loud = run(&repository, &["index", repository.root_str()]);
    let quiet = run(&repository, &["index", repository.root_str(), "--quiet"]);
    assert_eq!(loud.output.render(), quiet.output.render());
    assert!(!loud.output.progress.is_empty(), "index must narrate something");
    assert_eq!(
        quiet.output.progress.len(),
        loud.output.progress.len(),
        "the recorded narration is complete in both modes; --quiet silences the sink, not the \
         record"
    );
}

#[test]
fn every_command_records_its_narration_so_the_json_mode_is_complete() {
    // A JSON consumer that never receives the progress array cannot tell "nothing happened" from
    // "the caller asked for quiet".
    let repository = Repository::small("narration");
    let output = run(&repository, &["index", repository.root_str()]);
    assert!(
        output.output.progress.iter().any(|line| line.contains("generation")),
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
        (vec!["callers", "wallet_charge"], "callers", "walk", "callers"),
        (vec!["callees", "wallet_charge"], "callees", "walk", "callees"),
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
        assert_eq!(value["command"], envelope, "{command:?} named the wrong command");
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
    assert_eq!(outcome.output.refusal.as_ref().map(|r| r.kind.as_str()), Some(kind::NO_INDEX));
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
        Answer::Status(StatusAnswer { counts, states_partition, .. }) => {
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
    ] {
        assert!(joined.contains(expected), "status does not say: {expected}\n{joined}");
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
    let repository = Repository::empty("index-cold");
    let outcome = run(&repository, &["index", repository.root_str()]);
    assert_eq!(outcome.output.status, Status::Ok);
    assert_eq!(outcome.output.exit_code, 0);
    match &outcome.output.answer {
        Answer::Index(answer) => {
            assert_eq!(answer.mode, "build", "no generation exists, so this is a cold build");
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
    assert_eq!(second_answer.mode, "refresh", "a second run must not rebuild from scratch");
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
                answer.did_not.iter().any(|line| line.contains("crash between them")),
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
                answer.notes.iter().any(|note| note.contains("nothing to clear")),
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
    assert!(after < before, "the removed file's entities must be gone: {before} then {after}");
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
    assert_eq!(outcome.output.exit_code, 0, "one bad file must not fail the run");
    match &outcome.output.answer {
        Answer::Index(answer) => {
            assert!(
                answer.files_skipped > 0,
                "the undecodable file must be counted as skipped: {answer:?}"
            );
            assert!(
                answer.skipped.iter().any(|entry| entry.path.contains("broken.rs")),
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
            answer.findings.iter().any(|f| f.check == "generation" && f.summary.contains("empty")),
            "doctor must name the empty index: {answer:?}"
        ),
        other => panic!("expected a doctor answer, got {other:?}"),
    }
}

#[test]
fn a_run_over_a_path_that_is_not_a_directory_is_a_usage_error() {
    let repository = Repository::small("index-not-a-dir");
    let file = repository.root_str().to_owned() + "/src/ledger.rs";
    let outcome = run(&repository, &["index", &file]);
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
    let outcome = run(&repository, &["rm", "src/ledger.rs", "--root", repository.root_str()]);
    assert_eq!(outcome.output.exit_code, 0);
    match &outcome.output.answer {
        Answer::Remove(answer) => {
            assert_eq!(answer.scope, "file");
            assert_eq!(answer.indexed_paths_removed, 1, "one file was indexed at that path");
            assert!(answer.entities_removed > 0, "nothing was deleted: {answer:?}");
            assert!(
                answer.notes.iter().any(|note| note.contains("no_candidate")),
                "edges that pointed in are demoted, and that must be said: {:?}",
                answer.notes
            );
            assert!(
                answer.did_not.iter().any(|line| line.contains("did not touch the filesystem")),
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
    assert!(after < before, "relations must be gone: {before} then {after}");
}

#[test]
fn rm_on_a_subtree_removes_everything_at_or_below_it_and_says_so() {
    let repository = Repository::small("rm-subtree");
    run(&repository, &["index", repository.root_str()]);
    let outcome = run(&repository, &["rm", "src/ledger", "--root", repository.root_str()]);
    match &outcome.output.answer {
        Answer::Remove(answer) => {
            assert_eq!(answer.scope, "subtree");
            assert!(answer.indexed_paths_removed > 0, "nothing was covered: {answer:?}");
            assert!(
                answer.did_not.iter().any(|line| line.contains("whole subtree")),
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
    let outcome = run(&repository, &["rm", "src/never_seen.rs", "--root", repository.root_str()]);
    assert_eq!(outcome.output.exit_code, 0);
    match &outcome.output.answer {
        Answer::Remove(answer) => {
            assert_eq!(answer.indexed_paths_removed, 0);
            assert_eq!(answer.entities_removed, 0);
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
    assert!(refusal.message.contains(&repository.root_str()), "{}", refusal.message);
    assert!(refusal.message.contains("elsewhere"), "{}", refusal.message);
}

#[test]
fn rm_on_a_deleted_file_still_works_because_that_is_when_it_is_needed() {
    // The file is gone from disk and still in the index: the case a deletion creates. A command
    // that refused here would be useless exactly when it is useful.
    let repository = Repository::small("rm-deleted");
    run(&repository, &["index", repository.root_str()]);
    repository.remove("src/ledger.rs");
    let outcome = run(&repository, &["rm", "src/ledger.rs", "--root", repository.root_str()]);
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
    let outcome = run(&repository, &["rm", "src/a.rs", "--root", repository.root_str()]);
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
    let first = Repository::small("harness-a");
    let second = Repository::small("harness-b");
    assert_ne!(
        first.index_path(),
        second.index_path(),
        "two repositories must not share an index"
    );
    let path = first.index_path();
    run(&first, &["index", first.root_str()]);
    assert!(path.is_file(), "the build must have created the index at {path:?}");
    let _cleanup = Cleanup::on(path.clone());
    drop(first);
    assert!(!path.exists(), "dropping the repository must have removed its index");
}
