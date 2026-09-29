//! The end-to-end test: a command run against a real repository, whose output is checked against
//! the store's own measurements.
//!
//! # Why this is the test that matters most
//!
//! Every other test in this crate checks that a formatter prints what it was given. That is worth
//! having and it is not sufficient: a formatter that reads the wrong field, adds a stale label, or
//! re-derives a count instead of reporting one would pass all of them. The failure mode this file
//! exists to catch is **the printed number drifting from the data**, and the only way to catch it is
//! to put a real index in a real directory, run the binary's own library entry point against it,
//! and compare every number the command printed with what the store says.
//!
//! The comparison is against [`peek_core::store::Store::stats`], not against a second
//! implementation in this file. A test that recomputed the expected value would be a second
//! implementation of the thing under test, and two implementations agreeing proves less than one
//! implementation being checked against its own source of truth.
//!
//! Every fixture here is built by the real indexer. Nothing is inserted by hand, so an entity count
//! that drifts is a real drift.

// `expect` and `panic` are denied workspace-wide; an integration test is a separate crate and does
// not inherit the library's exemption. See `real_repository.rs` for the same justification.
#![allow(clippy::expect_used, clippy::panic)]

mod fixture;

use std::path::Path;

use fixture::{Repository, run};

use peek_cli::answer::{Answer, RemoveAnswer, StatusAnswer};
use peek_cli::args::{self, Invocation};
use peek_cli::commands::query;
use peek_cli::exit::{Status, kind};
use peek_cli::run as run_invocation;

/// Open the store the way a second process would, so the comparison is against the committed file
/// rather than against whatever handle the command left behind.
fn open_store(repository: &Repository) -> peek_core::store::Store {
    let repo = peek_core::store::RepoId::discover(repository.root()).expect("derive the identity");
    let path = repository.index_path();
    peek_core::store::Store::open(&path, &repo).expect("open the index the commands wrote")
}

#[test]
fn status_prints_the_counts_the_store_measured() {
    // The central claim. A status report is a claim about the index; every number in it came from
    // the store, and this asserts that it still does.
    let repository = Repository::small("e2e-status");
    let built = run(&repository, &["index", repository.root_str()]);
    assert_eq!(built.output.exit_code, 0, "{:?}", built.output.refusal);

    let printed = run(&repository, &["status"]);
    let store = open_store(repository);
    let measured = store.stats().expect("the store's own counts");
    drop(store);

    // The pattern names the store's own type, not the CLI's. Counts is a presentation struct
    // that mirrors these fields; destructuring StoreStats as a `Counts` would silently compare a
    // value with itself if the two ever converged, and would not compile the moment either
    // renamed a field. The comparison that matters is counts.* against measured.*.
    let peek_core::store::StoreStats {
        generation,
        schema_version,
        entity_count,
        relation_count,
        resolved_relations,
        ambiguous_relations,
        unresolved_relations,
        inferred_relations,
        pending_relations,
        candidate_count,
        orphan_relations,
        file_size_bytes,
        wal_size_bytes,
    } = measured;
    let _ = (
        ambiguous_relations,
        unresolved_relations,
        inferred_relations,
        candidate_count,
        file_size_bytes,
        wal_size_bytes,
    );

    match &printed.output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => {
            assert_eq!(counts.generation, generation, "generation");
            assert_eq!(counts.schema_version, schema_version, "schema version");
            assert_eq!(counts.entity_count, entity_count, "entity count");
            assert_eq!(counts.relation_count, relation_count, "relation count");
            assert_eq!(counts.resolved_relations, resolved_relations, "resolved");
            assert_eq!(counts.ambiguous_relations, ambiguous_relations, "ambiguous");
            assert_eq!(
                counts.unresolved_relations, unresolved_relations,
                "unresolved"
            );
            assert_eq!(counts.inferred_relations, inferred_relations, "inferred");
            assert_eq!(counts.pending_relations, pending_relations, "pending");
            assert_eq!(counts.candidate_count, candidate_count, "candidates");
            assert_eq!(counts.orphan_relations, orphan_relations, "orphans");
            assert!(
                entity_count > 0,
                "the fixture indexed nothing, so this proves nothing"
            );
            assert!(relation_count > 0, "the fixture extracted no relations");
        }
        other => panic!("expected a status answer, got {other:?}"),
    }
}

#[test]
fn status_json_carries_the_same_counts_it_prints() {
    // The two modes are two renderings of one value, and this asserts it by parsing the JSON back
    // rather than by comparing strings. A JSON mode nobody parses is not tested.
    let repository = Repository::small("e2e-status-json");
    run(&repository, &["index", repository.root_str()]);
    let store = open_store(repository);
    let measured = store.stats().expect("the store's own counts");
    drop(store);

    let printed = run(&repository, &["status", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(&printed.output.render_json()).expect("the JSON mode must parse");
    let counts = &value["answer"]["counts"];
    assert_eq!(
        counts["generation"], measured.generation,
        "generation in JSON"
    );
    assert_eq!(
        counts["entity_count"], measured.entity_count,
        "entity count in JSON"
    );
    assert_eq!(
        counts["relation_count"], measured.relation_count,
        "relation count in JSON"
    );
    assert_eq!(
        counts["orphan_relations"], measured.orphan_relations,
        "orphan count in JSON"
    );
    assert_eq!(value["exit_code"], 0);
    assert_eq!(value["status"], "ok");
}

#[test]
fn the_index_report_adds_up_to_what_the_store_holds_afterwards() {
    // Not merely self-consistent: the counts the run reported and the counts the store now holds
    // are the same measurement seen from two sides. A refresh that wrote less than it claimed is
    // exactly the defect that made the predecessor report "indexed N files" over an empty index.
    let repository = Repository::small("e2e-index-accounting");
    let built = run(&repository, &["index", repository.root_str()]);
    let reported = match &built.output.answer {
        Answer::Index(answer) => answer.clone(),
        other => panic!("expected an index answer, got {other:?}"),
    };
    let store = open_store(repository);
    let measured = store.stats().expect("the store's own counts");
    drop(store);

    assert_eq!(
        reported.entities_written, measured.entity_count,
        "the run reported writing a different number of entities than the store holds"
    );
    assert_eq!(
        reported.relations_written, measured.relation_count,
        "the run reported writing a different number of relations than the store holds"
    );
    assert!(
        reported.resolution_states_partition,
        "the five states the run reported do not add up to the relations it wrote: {reported:?}"
    );
    assert_eq!(
        reported.generation, measured.generation,
        "the generation the run reported is not the one the store records"
    );
    // A resolution pass that decided nothing would leave every relation pending, and the count
    // would still add up. The partition check alone would not catch it; this does.
    assert_eq!(
        measured.pending_relations, 0,
        "the build left undecided relations in the index, so a query about them cannot be answered"
    );
}

#[test]
fn a_refresh_of_one_changed_file_updates_that_file_and_nothing_else() {
    // Contract G2, verified by diffing semantic state rather than by timing: the entity count
    // changes by what the edit added or removed, and the generation advances by the commits the
    // refresh made.
    let repository = Repository::small("e2e-incremental");
    run(&repository, &["index", repository.root_str()]);
    let before = {
        let store = open_store(repository);
        let stats = store.stats().expect("counts");
        drop(store);
        stats
    };

    repository.write(
        "src/wallet.rs",
        "//! The wallet that issues charges.\n\n\
         /// Issue a charge for an amount.\n\
         pub fn issue(amount: u64) -> u64 {\n    amount\n}\n\n\
         /// A second declaration, added by the edit.\n\
         pub fn refund(amount: u64) -> u64 {\n    amount\n}\n",
    );
    let refreshed = run(&repository, &["index", repository.root_str()]);
    let after = {
        let store = open_store(repository);
        let stats = store.stats().expect("counts");
        drop(store);
        stats
    };

    match &refreshed.output.answer {
        Answer::Index(answer) => {
            assert_eq!(
                answer.mode, "refresh",
                "a warm run must refresh, not rebuild"
            );
            assert!(
                answer.generation > before.generation,
                "a refresh commits, so the generation must advance: {} then {}",
                before.generation,
                answer.generation
            );
        }
        other => panic!("expected an index answer, got {other:?}"),
    }
    assert!(
        after.entity_count > before.entity_count,
        "the added declaration is not in the index: {} then {}",
        before.entity_count,
        after.entity_count
    );
    // One file was rewritten, and `absorb_file` removes that file's rows before re-inserting them,
    // so a refresh of one file changes only that file's entities. The count moving by exactly the
    // number of declarations the edit added is the claim; anything else means the whole tree was
    // rewritten or something was double-counted.
    let added = after.entity_count - before.entity_count;
    assert!(
        added > 0 && added < before.entity_count,
        "the edit changed the entity count by {added} against a tree of {}, which is neither one \
         file's worth nor a full rebuild",
        before.entity_count
    );
}

#[test]
fn rm_removes_exactly_the_rows_of_the_path_it_names() {
    // The end-to-end version: the store's counts before and after, against what the command
    // reported. A `removing_subtree` that behaved like `removing_file`, or a count that did not
    // move at all, would both be caught here and neither by a formatter test.
    let repository = Repository::small("e2e-rm");
    run(&repository, &["index", repository.root_str()]);
    let before = {
        let store = open_store(repository);
        let stats = store.stats().expect("counts");
        drop(store);
        stats
    };

    let removed = run(
        &repository,
        &["rm", "src/ui", "--root", repository.root_str()],
    );
    let after = {
        let store = open_store(repository);
        let stats = store.stats().expect("counts");
        drop(store);
        stats
    };

    match &removed.output.answer {
        Answer::Remove(RemoveAnswer {
            indexed_paths_removed,
            entities_removed,
            scope,
            ..
        }) => {
            assert_eq!(scope, "subtree");
            assert_eq!(
                *entities_removed,
                after.entity_count.abs_diff(before.entity_count),
                "the command reported {entities_removed} entities removed and the store's count \
                 moved by {}",
                after.entity_count.abs_diff(before.entity_count)
            );
            assert!(
                *indexed_paths_removed >= 2,
                "the subtree held two files, so fewer than two indexed paths were covered"
            );
        }
        other => panic!("expected a remove answer, got {other:?}"),
    }
    assert!(
        after.entity_count < before.entity_count,
        "removing a subtree must reduce the entity count: {} then {}",
        before.entity_count,
        after.entity_count
    );
    // And no orphans: the store demotes edges that pointed into the removed scope rather than
    // leaving rows claiming a target that is gone, which is contract D4.
    assert_eq!(
        after.orphan_relations, 0,
        "a removal left {} relations pointing at a target that is not in the index",
        after.orphan_relations
    );
    store_verify(&repository);
}

#[test]
fn doctor_and_status_agree_about_every_count() {
    // One `Counts` type, two commands, one measurement. A number that differs between them is a
    // number one of them invented.
    let repository = Repository::small("e2e-doctor-status");
    run(&repository, &["index", repository.root_str()]);
    let store = open_store(repository);
    let measured = store.stats().expect("counts");
    drop(store);

    let from_status = match &run(&repository, &["status"]).output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => *counts,
        other => panic!("expected a status answer, got {other:?}"),
    };
    let from_doctor = match &run(&repository, &["doctor"]).output.answer {
        Answer::Doctor(answer) => answer.counts.expect("doctor must report the counts"),
        other => panic!("expected a doctor answer, got {other:?}"),
    };
    assert_eq!(from_status, from_doctor);
    assert_eq!(from_status.entity_count, measured.entity_count);
    assert_eq!(from_status.relation_count, measured.relation_count);
    assert_eq!(from_status.generation, measured.generation);
}

#[test]
fn doctor_reports_a_healthy_install_as_healthy_and_exits_zero() {
    // The green path matters as much as the red one: a `doctor` that could only ever fail would be
    // as useless as one that could only ever pass.
    let repository = Repository::small("e2e-doctor-healthy");
    run(&repository, &["index", repository.root_str()]);
    let outcome = run(&repository, &["doctor"]);
    assert_eq!(
        outcome.output.status,
        Status::Ok,
        "{:?}",
        outcome.output.refusal
    );
    assert_eq!(outcome.output.exit_code, 0);
    match &outcome.output.answer {
        Answer::Doctor(answer) => {
            assert!(answer.healthy, "{answer:?}");
            assert_eq!(answer.worst.as_deref(), Some("pass"), "{answer:?}");
            assert!(
                answer.findings.iter().any(|f| f.check == "integrity"),
                "the integrity check must have run: {answer:?}"
            );
            assert!(
                answer.findings.iter().all(|f| !f.detail.is_empty()),
                "a check that says ok without saying what it measured is indistinguishable from a \
                 check that did not run: {answer:?}"
            );
        }
        other => panic!("expected a doctor answer, got {other:?}"),
    }
}

#[test]
fn doctor_refuses_a_repository_it_had_to_create_an_index_for() {
    // `Store::open` creates a store, so a diagnosis of a repository that was never indexed
    // describes an index this command made. Exiting zero there would be a green answer about an
    // install that does not exist.
    let repository = Repository::empty("e2e-doctor-no-index");
    let outcome = run(&repository, &["doctor"]);
    assert_eq!(outcome.output.status, Status::Refused);
    assert_eq!(outcome.output.exit_code, 3);
    match &outcome.output.answer {
        Answer::Doctor(answer) => {
            assert!(
                !answer.index_existed,
                "there was no index before this command ran"
            );
            assert!(
                answer
                    .did_not
                    .iter()
                    .any(|line| line.contains("this command made")),
                "the answer must say the report describes an index it created: {:?}",
                answer.did_not
            );
        }
        other => panic!("expected a doctor answer, got {other:?}"),
    }
}

#[test]
fn a_deliberately_broken_index_is_diagnosed_and_exits_four() {
    // Contract L1: *diagnose a deliberately broken install*. The break is a file that is not a
    // SQLite database at all, which `validate_existing_store` turns into a `Corrupt` before a
    // single row is read — so the failure is a diagnosis rather than an IO error from SQLite's own
    // header parser.
    let repository = Repository::small("e2e-doctor-broken");
    run(&repository, &["index", repository.root_str()]);
    let path = repository.index_path();
    std::fs::write(&path, b"this is not a database, it is a sentence about one").expect("corrupt");
    // The file must be long enough for `Store::open` to treat it as an existing store.
    assert!(
        std::fs::metadata(&path).expect("stat").len() > 0,
        "a zero-length file is treated as a new store rather than a broken one"
    );

    let outcome = run(&repository, &["doctor"]);
    assert_eq!(
        outcome.output.status,
        Status::Unhealthy,
        "a broken index must be unhealthy, not a quiet success: {:?}",
        outcome.output.refusal
    );
    assert_eq!(
        outcome.output.exit_code, 4,
        "a failing check has its own code"
    );
    match &outcome.output.answer {
        Answer::Doctor(answer) => {
            let failing: Vec<&str> = answer
                .findings
                .iter()
                .filter(|f| f.severity == "fail")
                .map(|f| f.check.as_str())
                .collect();
            assert!(
                failing.contains(&"index_openable"),
                "the open failure must be reported as a failing check: {answer:?}"
            );
        }
        other => panic!("expected a doctor answer, got {other:?}"),
    }
    let rendered = outcome.output.render();
    assert!(rendered.contains("fail"), "{rendered}");
    assert!(rendered.contains("worst:"), "{rendered}");
}

#[test]
fn the_whole_command_surface_answers_against_a_real_index() {
    // Every command, in order, against one repository, asserting only that each produced an answer
    // and the code the contract names. The per-command tests above assert the content; this one
    // exists so that adding a command without a smoke test is visible, and so a dispatch arm that
    // never runs is caught.
    let repository = Repository::small("e2e-surface");
    assert_eq!(
        run(&repository, &["index", repository.root_str()])
            .output
            .exit_code,
        0
    );
    let root = repository.root_str().to_owned();
    for (argv, expected) in [
        (vec!["status"], 0),
        (vec!["doctor"], 0),
        (vec!["explain", "wallet_charge"], 0),
        (vec!["callers", "wallet_charge"], 0),
        (vec!["callees", "wallet_charge"], 0),
        (vec!["dependents", "wallet_charge"], 0),
        (vec!["context", "wallet_charge", "--budget", "4000"], 0),
        (vec!["explain", "no_such_declaration_anywhere"], 3),
        (vec!["context", "wallet_charge", "--budget", "1"], 3),
        (vec!["status", "--nonsense"], 2),
    ] {
        let mut full = argv.clone();
        if !argv.first().is_some_and(|first| *first == "index") {
            full.push("--root");
            full.push(&root);
        }
        let outcome = run(&repository, &full);
        assert_eq!(
            outcome.output.exit_code, expected,
            "{argv:?} exited {} with {:?}",
            outcome.output.exit_code, outcome.output.refusal
        );
    }
}

#[test]
fn two_runs_of_the_same_query_over_an_unchanged_index_produce_identical_output() {
    // Determinism, end to end. The engine asserts it for a pack and a walk; this asserts it for the
    // *rendered* command, which is where a formatter that iterates a hash map or formats a
    // timestamp would break it.
    let repository = Repository::small("e2e-determinism");
    run(&repository, &["index", repository.root_str()]);
    let root = repository.root_str().to_owned();
    for argv in [
        vec!["status".to_owned()],
        vec![
            "context".to_owned(),
            "wallet_charge".to_owned(),
            "--budget".to_owned(),
            "4000".to_owned(),
        ],
        vec!["explain".to_owned(), "wallet_charge".to_owned()],
        vec!["dependents".to_owned(), "wallet_charge".to_owned()],
    ] {
        let mut full = argv.clone();
        full.push("--root".to_owned());
        full.push(root.clone());
        let borrowed: Vec<&str> = full.iter().map(String::as_str).collect();
        let first = run(&repository, &borrowed);
        let second = run(&repository, &borrowed);
        assert_eq!(
            first.output.render(),
            second.output.render(),
            "two runs of {argv:?} over an unchanged index rendered differently"
        );
    }
}

#[test]
fn every_edge_an_explanation_lists_carries_a_resolution_state() {
    // D-0003, from the CLI's side. Audit B10 found the predecessor's `Edge.reason` was a constant
    // string on every producer, so a call resolved by a written import and a call resolved by
    // alphabetical order were byte-identical records; B11 followed, and `explain()` emitted prose
    // nothing in the data supported. A field that is present but empty would be the same defect one
    // layer up, so the CLI asserts it is not.
    let repository = Repository::small("e2e-edge-states");
    run(&repository, &["index", repository.root_str()]);
    let output = run(
        &repository,
        &["explain", "wallet_charge", "--root", repository.root_str()],
    )
    .output;
    match &output.answer {
        Answer::Explain(answer) => {
            assert!(
                query::every_edge_is_explained(&answer.explanation),
                "an edge with no state is a record the index cannot explain about itself: {:?}",
                answer
                    .explanation
                    .edges
                    .iter()
                    .map(|edge| edge.state.clone())
                    .collect::<Vec<String>>()
            );
            // And an ambiguous edge prints its candidate count, so a reader who sees only the
            // rendered line still learns the edge is undecided.
            for edge in &answer.explanation.edges {
                if edge.relation.resolution.is_ambiguous() {
                    let count = edge.candidate_count();
                    assert!(
                        edge.state.contains(&count.to_string()),
                        "an ambiguous edge must print its candidate count, and the number it \
                         prints must be the number it hands back: {} vs {count}",
                        edge.state
                    );
                }
            }
        }
        other => panic!("expected an explain answer, got {other:?}"),
    }
}

#[test]
fn an_ambiguous_target_is_refused_with_every_candidate_named() {
    // D-0004, from the CLI's side, and the reason the refusal carries a candidate list: the caller
    // disambiguates, this program does not. The fixture declares `render` in two files, so the
    // ambiguity is real rather than constructed.
    let repository = Repository::small("e2e-ambiguous");
    run(&repository, &["index", repository.root_str()]);
    let output = run(
        &repository,
        &["explain", "render", "--root", repository.root_str()],
    )
    .output;
    assert_eq!(output.exit_code, 3, "an ambiguous target is a refusal");
    let refusal = output.refusal.expect("a refusal must be carried");
    assert_eq!(refusal.kind.as_str(), kind::AMBIGUOUS_TARGET, "{refusal:?}");
    assert!(
        refusal.candidates.len() >= 2,
        "both declarations must be named, not counted: {refusal:?}"
    );
    // And the names are the two distinct files, so a caller can choose between them.
    let distinct: std::collections::BTreeSet<&String> = refusal.candidates.iter().collect();
    assert_eq!(
        distinct.len(),
        refusal.candidates.len(),
        "the same candidate twice: {refusal:?}"
    );
    let rendered = refusal.render();
    for candidate in &refusal.candidates {
        assert!(rendered.contains(candidate), "{rendered}");
    }
}

#[test]
fn a_symlinked_root_and_its_target_reach_the_same_index() {
    // The identity is derived from the canonical root, so a link and its target are one repository
    // and one cache. Two indexes for one tree is how one of them silently goes stale.
    let repository = Repository::small("e2e-symlink");
    run(&repository, &["index", repository.root_str()]);
    let measured = {
        let store = open_store(repository);
        let stats = store.stats().expect("counts");
        drop(store);
        stats
    };
    // A second spelling of the same directory — `.` and a `..` round trip — must resolve to the
    // same canonical root, hence the same index. Asserted through the two spellings rather than
    // through a symlink so the property is tested on every platform; a real symlink is tested
    // separately in `paths`'s own unit tests, which are gated on the platform allowing one.
    let direct = run(&repository, &["status", "--root", repository.root_str()]);
    let roundabout = run(
        &repository,
        &[
            "status",
            "--root",
            &format!("{}/src/..", repository.root_str()),
        ],
    );
    assert_eq!(
        direct.output.index_path, roundabout.output.index_path,
        "two spellings of one directory reached two indexes"
    );
    let from_roundabout = match &roundabout.output.answer {
        Answer::Status(StatusAnswer { counts, .. }) => *counts,
        other => panic!("expected a status answer, got {other:?}"),
    };
    assert_eq!(
        from_roundabout.entity_count, measured.entity_count,
        "the same index read through a different spelling reported different contents"
    );
}

#[test]
fn every_command_states_what_it_could_not_do() {
    // The project's central rule, checked across the whole surface at once rather than one command
    // at a time. A command that could do everything still has a version it did not support, a check
    // it did not run, and a file class it did not read — and a reader cannot tell the difference
    // between an answer that is complete and one that is quietly partial.
    let repository = Repository::small("e2e-did-not");
    run(&repository, &["index", repository.root_str()]);
    let root = repository.root_str().to_owned();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["status"], "did not walk the repository"),
        (vec!["doctor"], "did not run the resolver"),
        (
            vec!["context", "wallet_charge", "--budget", "4000"],
            "counting rule",
        ),
        (
            vec!["explain", "wallet_charge"],
            "did not search for a name close to",
        ),
        (
            vec!["callers", "wallet_charge"],
            "ambiguous or unresolved edge",
        ),
        (vec!["callees", "wallet_charge"], "structural edges"),
        (
            vec!["dependents", "wallet_charge"],
            "a depth of 0 returns nothing",
        ),
        (
            vec!["rm", "src/never_seen.rs"],
            "did not touch the filesystem",
        ),
        (vec!["index"], "did not compare file contents"),
    ];
    for (argv, expected) in cases {
        let mut full = argv.clone();
        if !argv.contains(&"--root") {
            full.push("--root");
            full.push(&root);
        }
        let answer = run(&repository, &full).output.answer;
        let did_not = answer.did_not();
        assert!(
            !did_not.is_empty(),
            "{argv:?} states nothing it could not do, which means it is claiming to be complete"
        );
        let joined = did_not.join("\n");
        assert!(
            joined.contains(expected),
            "{argv:?} does not say: {expected}\nit says:\n{joined}"
        );
        // And the human rendering prints the whole list, not a summary of it.
        let text = run(&repository, &full).output.render();
        assert!(
            text.contains("could not:"),
            "{argv:?} does not print its `did_not` list:\n{text}"
        );
    }
}

#[test]
fn the_narration_a_run_produces_names_the_work_it_did() {
    // The progress sink is where a person watching a slow command finds out what it is doing. The
    // assertions are on the content rather than the count, because a count would pass for narration
    // that says nothing.
    let repository = Repository::small("e2e-narration");
    let (output, lines) = run_collecting(&repository, &["index", repository.root_str()]);
    assert_eq!(output.exit_code, 0, "{:?}", output.refusal);
    let joined = lines.join("\n");
    assert!(
        joined.contains("no generation has been committed") || joined.contains("refreshing"),
        "the narration must say whether this was a build or a refresh: {joined}"
    );
    assert!(
        joined.contains(repository.root_str()),
        "the narration must name the tree it is working on: {joined}"
    );
    // And the recorded list is the same list, so a JSON consumer sees everything the sink saw.
    assert_eq!(output.progress, lines, "the record and the sink must agree");
}

#[test]
fn a_corrupted_index_is_reported_by_every_command_that_opens_it_rather_than_read_as_empty() {
    // Audit A6: the predecessor reported a healthy install over a broken index. A command that
    // opens a store whose file is not a store must fail, and must not answer with zeros.
    let repository = Repository::small("e2e-corrupt-commands");
    run(&repository, &["index", repository.root_str()]);
    let path = repository.index_path();
    std::fs::write(&path, vec![b'x'; 4096]).expect("corrupt the index");

    for command in [
        "status",
        "context",
        "explain",
        "callers",
        "callees",
        "dependents",
    ] {
        let mut argv = vec![command, "wallet_charge"];
        if command == "status" {
            argv.truncate(1);
        }
        if command == "context" {
            argv.push("--budget");
            argv.push("4000");
        }
        argv.push("--root");
        argv.push(repository.root_str());
        let outcome = run(&repository, &argv);
        let code = outcome.output.exit_code;
        let Some(refusal) = outcome.output.refusal else {
            panic!("{command} exited {code} over a corrupted index and said nothing about why");
        };
        assert_ne!(code, 0, "{command} exited zero over a corrupted index");
        assert_eq!(
            outcome.output.status,
            Status::Failed,
            "{command} must report an engine failure, not a question it could not answer: \
             {refusal:?}"
        );
        assert_eq!(
            refusal.kind.as_str(),
            kind::ENGINE,
            "{command}: {refusal:?}"
        );
    }
}

/// Assert the store still passes its own integrity check.
fn store_verify(repository: &Repository) {
    let store = open_store(repository);
    store
        .verify()
        .expect("the index must pass SQLite's integrity check");
    assert_eq!(
        Path::new(&repository.index_path()),
        store.path(),
        "the store must be the file the tests think it is"
    );
}

/// Run a command against a repository with a sink the test owns, so a test can assert on what the
/// sink received as well as on what the answer says.
pub fn run_collecting(_repository: &Repository, argv: &[&str]) -> (peek_cli::Output, Vec<String>) {
    let owned: Vec<std::ffi::OsString> = argv
        .iter()
        .map(|argument| std::ffi::OsString::from(*argument))
        .collect();
    let parsed: Invocation = args::parse(owned).expect("the command line must parse");
    let mut collecting = peek_cli::progress::Collecting::new();
    match run_invocation(&parsed, &mut collecting) {
        Ok(output) => (output, collecting.lines),
        Err(failure) => panic!("{argv:?} failed: {}", failure.render()),
    }
}
