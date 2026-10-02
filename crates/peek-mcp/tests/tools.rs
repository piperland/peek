//! The tool handlers, driven directly, against a real indexed repository.
//!
//! # The fixture
//!
//! A small Rust crate, written to disk and indexed by the real pipeline — real discovery, real
//! Tree-sitter extraction, real resolution, real SQLite. Not a hand-built graph, because the claim
//! under test is that the MCP surface routes to the engine and reports what the engine did, and a
//! hand-built graph would be testing the reporting against a graph nobody produced.
//!
//! Two relations are then added to that real index deliberately, because two claims cannot be
//! provoked from a four-file crate by writing plausible Rust:
//!
//! * an **ambiguous** call, so a test can assert that an ambiguous edge reaches the caller with its
//!   candidate list rather than as a fact;
//! * an **unresolved** call to something outside the repository.
//!
//! They are added through the engine's own write path, so they are ordinary rows, and they are added
//! *after* the resolution pass so nothing re-decides them. Their endpoints are located by reading
//! the index rather than by naming them, so a change to the extractor's output cannot turn this into
//! a fixture bug that reads like a product defect.
//!
//! # What is asserted, and how
//!
//! Every response is parsed back with `serde_json` and asserted on by structure. A response nobody
//! parses is not tested. Where a test needs a number it derives it from the engine — a budget from
//! `minimum_budget`, a threshold from a first answer — rather than writing a literal nobody measured.

// `expect` and `panic` are denied workspace-wide, on the grounds that in production code they hide
// a real failure behind a panic. `peek-core`'s unit tests are exempted through that crate's
// `lib.rs`; an integration test is a separate crate and does not inherit that, so it is exempted
// here instead. The justification is the same one: a test that fails inside an `expect` has
// already failed, and a message naming what went wrong is worth more than a panic location.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use peek_core::model::entity::{EntityId, EntityKind};
use peek_core::model::relation::{Relation, RelationKind, UnresolvedReason};
use peek_core::model::span::Span;
use peek_core::store::{self, IndexUpdate, RepoId, Store};
use serde_json::{Value, json};

use peek_mcp::Session;
use peek_mcp::session::SharedLog;
use peek_mcp::tools::{self, ToolAnswer};
use peek_mcp::{Outcome, ToolError};

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// A repository, its index, and the handles needed to remove both.
struct Fixture {
    root: TempDir,
    session: Session,
    log: SharedLog,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        remove_index(self.root.path());
    }
}

/// Remove the index a repository's identity names.
///
/// The index lives outside the repository, under the OS cache root, named for the repository's
/// identity. Removing it here is the only reason these tests leave nothing in a developer's cache.
fn remove_index(root: &Path) {
    if let Ok(repo) = RepoId::discover(root) {
        if let Ok(directory) = store::paths::index_dir(&repo) {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("peek-mcp-fixture-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create a temporary repository");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create a parent directory");
        }
        std::fs::write(path, contents).expect("write a source file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        remove_index(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The crate every test indexes. Four files, one call chain, and one function that is deliberately
/// hard to place.
///
/// ```text
///   handler ──calls──▶ process ──calls──▶ settle ──calls──▶ reconcile
///                            │                ▲
///                            └──calls──▶ audit┘
///
///   process ──calls──▶ render        ambiguous between service.rs and ui.rs
///   process ──calls──▶ external      unresolved: it lives outside this repository
/// ```
fn write_sources(root: &TempDir) {
    root.write(
        "src/ledger.rs",
        "\
pub fn settle() -> u32 {
    reconcile()
}

pub fn reconcile() -> u32 {
    1
}

pub fn audit() -> u32 {
    settle()
}
",
    );
    root.write(
        "src/service.rs",
        "\
use crate::ledger::{audit, settle};

pub struct Gateway;

pub fn process() -> u32 {
    settle();
    audit();
    render();
    0
}

pub fn render() -> u32 {
    2
}
",
    );
    root.write(
        "src/ui.rs",
        "\
pub fn render() -> u32 {
    3
}
",
    );
    root.write(
        "src/api.rs",
        "\
use crate::service::process;

pub fn handler() -> u32 {
    process()
}
",
    );
    root.write(
        "Cargo.toml",
        "\
[package]
name = \"fixture-payments\"
version = \"0.1.0\"
edition = \"2021\"
",
    );
}

/// A session over a real, fully indexed repository.
fn indexed(label: &str) -> Fixture {
    let root = TempDir::new(label);
    write_sources(&root);
    let log = SharedLog::new();
    let mut session = Session::new(root.path(), Box::new(log.clone()));

    let build = call(&mut session, "index", json!({ "mode": "full" }));
    let report = &build["report"];
    assert_eq!(
        report["states_partition"],
        json!(true),
        "the fixture must build a partitioning index or nothing else is worth testing: {build}"
    );

    plant_uncertainty(&mut session);
    Fixture { root, session, log }
}

/// A session over a repository that has real source but no index.
fn unindexed(label: &str) -> Fixture {
    let root = TempDir::new(label);
    write_sources(&root);
    let log = SharedLog::new();
    let session = Session::new(root.path(), Box::new(log.clone()));
    Fixture { root, session, log }
}

/// Add the one ambiguous and the one unresolved relation the real extractor will not produce here.
///
/// Written through the engine's own write path, so they are ordinary rows, and written *after* the
/// resolution pass so that no later pass re-decides them.
fn plant_uncertainty(session: &mut Session) {
    let mut store = session.writer().expect("a write handle for the fixture");
    let mut update = IndexUpdate::empty();

    let service = entity_in(&store, "src/service.rs", EntityKind::Function, "process");
    let own_render = entity_in(&store, "src/service.rs", EntityKind::Function, "render");
    let ui_render = entity_in(&store, "src/ui.rs", EntityKind::Function, "render");
    assert_ne!(
        own_render, ui_render,
        "the fixture's ambiguity needs two `render` declarations in two files, and found only \
         {own_render}"
    );

    let span = Span::new(0, 10, 1, 1, 1, 11).expect("a valid span");
    // Both planted relations are calls *from* the same function, so the first takes a copy of the
    // identity and the second takes the original. The identity is the same entity either way.
    update = update.with_relation(Relation::ambiguous(
        RelationKind::Calls,
        service.clone(),
        "render",
        span,
        vec![own_render, ui_render],
    ));
    update = update.with_relation(Relation::unresolved(
        RelationKind::Calls,
        service,
        "std::io::read_to_string",
        span,
        UnresolvedReason::External,
    ));
    store
        .apply_update(update)
        .unwrap_or_else(|error| panic!("the fixture's relations could not be written: {error}"));
}

/// One indexed entity, located by file, kind and qualified name.
fn entity_in(store: &Store, file: &str, kind: EntityKind, qualified_name: &str) -> EntityId {
    let path = peek_core::model::RepoPath::new(file).expect("a valid repository path");
    let declared = store
        .entities_in_file(&path, 2_000)
        .unwrap_or_else(|error| panic!("reading {file} failed: {error}"));
    // Borrowed, not consumed: the message below names what *was* in the file, so moving the list
    // into `into_iter` would take away the evidence for the panic it is meant to explain.
    declared
        .iter()
        .find(|entity| entity.kind() == kind && entity.id.qualified_name() == qualified_name)
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the fixture has no {kind} `{qualified_name}` in {file}; the indexed entities \
                 there are {:?}",
                declared
                    .iter()
                    .map(|entity| entity.id.qualified_name())
                    .collect::<Vec<_>>()
            )
        })
        .id
}

/// Run a tool and return its structured content **and** its text rendering, failing if it refused.
///
/// Both halves, because they are two channels and a test that reads one of them is not testing
/// what the tool sent. `structured` is what a program parses and `text` is what a client renders;
/// the text is *not* a field of the structured body, so a test that wants the prose a reader would
/// see has to take it from the `ToolAnswer` rather than looking for it under `structured` — where
/// its absence means nothing and its presence would mean a tool had duplicated itself.
fn call_both(session: &mut Session, name: &str, arguments: Value) -> (Value, String) {
    match tools::dispatch(session, name, Some(&arguments)) {
        Ok(ToolAnswer {
            text, structured, ..
        }) => (structured, text),
        Err(error) => panic!(
            "`{name}` refused a call the test expected to work: {} ({:?})",
            error.verdict_reason, error.outcome
        ),
    }
}

/// Run a tool and return its structured content, failing the test if it refused.
fn call(session: &mut Session, name: &str, arguments: Value) -> Value {
    call_both(session, name, arguments).0
}

/// Run a tool and return the refusal, failing the test if it succeeded.
fn refuse(session: &mut Session, name: &str, arguments: Value) -> ToolError {
    match tools::dispatch(session, name, Some(&arguments)) {
        Ok(ToolAnswer { structured, .. }) => {
            panic!("`{name}` answered a call the test expected to be refused: {structured}")
        }
        Err(error) => error,
    }
}

/// The five resolution states, as numbers, whether they partition or not.
fn states(report: &Value) -> [u64; 5] {
    let states = &report["resolution_states"];
    [
        states["resolved"].as_u64().expect("a count"),
        states["inferred"].as_u64().expect("a count"),
        states["ambiguous"].as_u64().expect("a count"),
        states["unresolved"].as_u64().expect("a count"),
        states["pending"].as_u64().expect("a count"),
    ]
}

/// The qualified names of the units in a pack, in the order the pack ranked them.
fn unit_names(pack: &Value) -> Vec<String> {
    pack["pack"]["units"]
        .as_array()
        .expect("a pack carries its units")
        .iter()
        .filter_map(|unit| {
            unit["entity"]["id"]["qualified_name"]
                .as_str()
                .map(ToOwned::to_owned)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// index
// ---------------------------------------------------------------------------

#[test]
fn a_full_build_reports_all_five_resolution_states_and_they_partition() {
    let mut fixture = indexed("index-states");
    let built = call(&mut fixture.session, "index", json!({ "mode": "full" }));
    let report = &built["report"];
    for key in ["resolved", "pending", "ambiguous", "unresolved", "inferred"] {
        assert!(
            report["resolution_states"][key].as_u64().is_some(),
            "all five states are present, `{key}` included: {report}"
        );
    }
    assert_eq!(
        report["resolution_states"]["measured"],
        json!("run_as_extracted"),
        "a build says its five counts are what the extractor left, so they cannot be read as a \
         claim about the index: {report}"
    );
    let sum: u64 = states(report).iter().sum();
    assert_eq!(
        sum,
        report["relations_written"].as_u64().expect("a count"),
        "the five states account for every relation the build wrote"
    );
    assert_eq!(report["states_partition"], json!(true));
    assert!(
        report["skipped"].is_array(),
        "the skipped list is present even when it is empty: {report}"
    );
    assert!(
        report["resolution"].is_object(),
        "the resolution pass's own report travels with the build's: {report}"
    );

    // The report carries two sets of state numbers and they measure **disjoint populations**, which
    // is the thing a reader cannot see and the thing this test exists to pin.
    //
    // `resolution_states` is what the *extractor* wrote: the relations it settled itself as it went
    // (structural `contains` edges need no resolution) and the relations it handed to the second
    // pass as `Pending`. `resolution` is what the second pass did with those pending ones. So
    // `resolution_states.pending` is work the pass was *given*, not work the index still has, and a
    // reader who takes it for the index's state reads a build that resolved everything as a build
    // with nine relations outstanding. `pending_remaining` is the field that says which is which.
    let resolution = &report["resolution"];
    assert_eq!(
        resolution["pending_remaining"],
        json!(false),
        "nothing is left undecided, which is what makes `pending` above an input rather than a \
         debt: {report}"
    );
    assert_eq!(
        report["resolution_states"]["resolved"]
            .as_u64()
            .expect("a count")
            + resolution["resolved"].as_u64().expect("a count"),
        report["relations_written"].as_u64().expect("a count"),
        "the relations the extractor settled and the ones the pass settled are additive, not \
         competing readings of one number: {report}"
    );
}

#[test]
fn a_full_build_names_every_file_it_refused_to_index() {
    let root = TempDir::new("index-skipped");
    write_sources(&root);
    // A file that is not valid UTF-8. Discovery refuses it, and the report has to say so: a build
    // that reported a clean run over a tree it could not read is the defect this engine removes.
    std::fs::write(root.path().join("src/broken.rs"), [0xff, 0xfe, 0x00])
        .expect("write a file that is not UTF-8");

    let mut session = Session::new(root.path(), Box::new(SharedLog::new()));
    let (built, text) = call_both(&mut session, "index", json!({ "mode": "full" }));
    let report = &built["report"];
    assert!(
        report["files_skipped"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "a file that cannot be decoded is counted as skipped: {report}"
    );
    assert!(
        report["skipped"].is_array(),
        "the list is present even when it is empty: {report}"
    );
    // The file is named in the structured list, in the rendered report, or in both. Which of the two
    // carries it is the engine's business; that it is named at all is the contract. The rendered
    // report is the `ToolAnswer`'s text rather than a member of the structured body, so it is read
    // from the answer — a `text` looked up under `structured` would be `null` for every tool and
    // would quietly make this half of the disjunction dead.
    let named_in_list = report["skipped"]
        .as_array()
        .expect("an array")
        .iter()
        .any(|file| {
            file["path"]
                .as_str()
                .is_some_and(|p| p.contains("broken.rs"))
        });
    let named_in_text = text.contains("broken.rs");
    assert!(
        named_in_list || named_in_text,
        "a file that was not indexed is named, not merely counted: {report}"
    );
}

#[test]
fn index_status_reports_where_the_index_is_and_never_inside_the_repository() {
    let mut fixture = indexed("status-location");
    let status = call(&mut fixture.session, "index_status", json!({}));
    let index_path = status["index_path"].as_str().expect("a path");
    assert!(
        !Path::new(index_path).starts_with(fixture.root.path()),
        "the index must live under the OS cache root and never inside the tree it describes \
         (D-0006): {index_path}"
    );
    assert!(
        index_path.ends_with("index.db"),
        "the file is named: {index_path}"
    );
    assert_eq!(status["states_partition"], json!(true));
    assert_eq!(
        status["resolution_states"]["measured"],
        json!("index_as_stored"),
        "a status call says its five counts are the store's own measurement, so the two tools that \
         carry the same five names under the same key cannot be read as one number: {status}"
    );
    assert!(
        status["stats"]["entity_count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "a real index holds entities: {status}"
    );
    assert_eq!(status["durability"], json!("FULL"));
}

#[test]
fn index_status_says_an_unchanged_handle_is_current() {
    // The other half of the claim, and the half that is deterministic: a session that has not
    // written anything since it opened its reader is not behind, and the tool says so rather than
    // leaving the field to be guessed at.
    let mut fixture = indexed("status-generation");
    let first = call(&mut fixture.session, "index_status", json!({}));
    let second = call(&mut fixture.session, "index_status", json!({}));
    assert_eq!(
        first["opened_at_generation"], first["generation"],
        "the handle is opened before the figure is read, so the first call already knows it: \
         {first}"
    );
    assert_eq!(first["handle_is_stale"], json!(false));
    assert_eq!(
        first["recorded_generation"], first["opened_at_generation"],
        "and the index records what the handle was opened at, because nothing has: {first}"
    );
    assert_eq!(
        second, first,
        "and nothing changed between the two: {second}"
    );
}

#[test]
fn index_status_says_the_handle_is_behind_once_anything_commits() {
    // The claim that the flag is for, and the reason it exists: the session's reader is opened once
    // and kept, so its cached generation stops being true the moment anything commits beside it.
    //
    // A `refresh` is used rather than a watch because it is the same situation with none of the
    // timing in it. It opens its own short-lived writer, commits and closes, and the reader this
    // session is holding is untouched — which is precisely the condition the field reports. A
    // refresh that returned the same generation as the handle would mean the comparison in the
    // tool is reading the cache twice, which is the defect this replaces.
    let mut fixture = indexed("status-stale");
    let before = call(&mut fixture.session, "index_status", json!({}));
    assert_eq!(before["handle_is_stale"], json!(false));

    fixture.root.write(
        "src/ui.rs",
        "\
pub fn render() -> u32 {
    30
}

pub fn decorate() -> u32 {
    4
}
",
    );
    let refreshed = call(
        &mut fixture.session,
        "index",
        json!({ "mode": "refresh", "paths": ["src/ui.rs"] }),
    );
    assert_eq!(
        refreshed["outcome"],
        json!("ok"),
        "the refresh applied: {refreshed}"
    );

    let after = call(&mut fixture.session, "index_status", json!({}));
    assert_eq!(
        after["opened_at_generation"], before["opened_at_generation"],
        "the handle is still the one this session opened, and it still reports what it was opened \
         at: {after}"
    );
    assert_eq!(
        after["recorded_generation"], refreshed["report"]["generation"],
        "the index records the generation the refresh committed: {after}"
    );
    assert_eq!(
        after["handle_is_stale"],
        json!(true),
        "and the tool says the handle is behind rather than serving the cached figure as current: \
         {after}"
    );
}

#[test]
fn a_query_tool_on_an_unindexed_repository_says_not_indexed_and_creates_nothing() {
    let mut fixture = unindexed("not-indexed");
    for name in [
        "index_status",
        "explain",
        "callers",
        "callees",
        "dependents",
    ] {
        let mut arguments = json!({});
        if name != "index_status" {
            arguments["target"] = json!("process");
        }
        let error = refuse(&mut fixture.session, name, arguments);
        assert_eq!(
            error.outcome,
            Outcome::NotIndexed,
            "`{name}` must distinguish 'never indexed' from 'nothing found'"
        );
        assert_eq!(
            error.advice.as_deref(),
            Some("run the `index` tool first"),
            "the refusal names the tool that fixes it"
        );
    }
    // `context` needs a budget, so it is asked with one rather than being refused for the budget.
    let error = refuse(
        &mut fixture.session,
        "context",
        json!({ "target": "process", "budget_tokens": 100_000 }),
    );
    assert_eq!(error.outcome, Outcome::NotIndexed);

    let path = fixture
        .session
        .index_path()
        .expect("an index location resolves even when nothing is there")
        .to_path_buf();
    assert!(
        !path.exists(),
        "asking a question must not create an index at {}",
        path.display()
    );
}

#[test]
fn a_refresh_updates_only_the_paths_it_is_given() {
    let mut fixture = indexed("refresh-scoped");
    let before = call(&mut fixture.session, "index_status", json!({}));
    let generation = before["generation"].as_u64().expect("a generation");

    fixture.root.write(
        "src/ui.rs",
        "\
pub fn render() -> u32 {
    30
}

pub fn decorate() -> u32 {
    4
}
",
    );

    let refreshed = call(
        &mut fixture.session,
        "index",
        json!({ "mode": "refresh", "paths": ["src/ui.rs"] }),
    );
    let report = &refreshed["report"];
    assert_eq!(
        report["files_indexed"].as_u64(),
        Some(1),
        "one file was named and one was re-indexed: {report}"
    );
    assert_eq!(
        report["states_partition"],
        json!(true),
        "a refresh must leave the index partitioning: {report}"
    );
    // `generation` is the generation *this session's handle* was opened at, and `index_status`
    // reports it beside `handle_is_stale` rather than serving it as current. The number that moves
    // when a refresh commits is `recorded_generation`, read from the database rather than from the
    // handle's cache. Asserting the other one would be asserting the staleness the tool now names,
    // and the refresh *is* in the index either way — which the `dependents` call below is what
    // actually checks.
    let after = call(&mut fixture.session, "index_status", json!({}));
    assert_eq!(
        after["opened_at_generation"],
        json!(generation),
        "the handle is still the one the status call above opened, and it still reports what it was \
         opened at: {after}"
    );
    assert!(
        after["recorded_generation"].as_u64().expect("a generation") > generation,
        "and the index records a later one, because a refresh committed beside it: {after}"
    );
    assert_eq!(
        after["handle_is_stale"],
        json!(true),
        "so the tool says the handle is behind rather than reporting a cached figure as current: \
         {after}"
    );
    // The new function is answerable, which is the point of the refresh rather than of the report.
    let found = call(
        &mut fixture.session,
        "dependents",
        json!({ "target": "decorate", "depth": 1 }),
    );
    assert_eq!(
        found["outcome"],
        json!("ok"),
        "the refreshed function is indexed: {found}"
    );
}

#[test]
fn a_refresh_of_a_path_that_no_longer_exists_removes_it_from_the_index() {
    let mut fixture = indexed("refresh-delete");
    // Before and after the removal, `decorate` is not in the index, and a target that is not in the
    // index is a **refusal** — `unknown_target`, carrying the three-lookup detail — not an answer
    // with an `outcome` member. `an_unknown_target_names_which_of_the_three_lookups_missed` pins
    // that for `dependents` among the rest; asking for the same thing through the helper that
    // panics on a refusal is what made this test die before it reached the refresh at all.
    let before = refuse(
        &mut fixture.session,
        "dependents",
        json!({ "target": "decorate", "depth": 1 }),
    );
    assert_eq!(
        before.outcome,
        Outcome::UnknownTarget,
        "the fixture does not have this function yet: {}",
        before.verdict_reason
    );
    fixture.root.write(
        "src/ui.rs",
        "\
pub fn render() -> u32 {
    3
}

pub fn decorate() -> u32 {
    4
}
",
    );
    call(
        &mut fixture.session,
        "index",
        json!({ "mode": "refresh", "paths": ["src/ui.rs"] }),
    );
    let found = call(
        &mut fixture.session,
        "dependents",
        json!({ "target": "decorate", "depth": 1 }),
    );
    assert_eq!(
        found["outcome"],
        json!("ok"),
        "the added function is indexed: {found}"
    );

    std::fs::remove_file(fixture.root.path().join("src/ui.rs")).expect("delete one file");
    let removed = call(
        &mut fixture.session,
        "index",
        json!({ "mode": "refresh", "paths": ["src/ui.rs"] }),
    );
    assert_eq!(
        removed["report"]["files_removed"].as_u64(),
        Some(1),
        "a path that is gone is removed rather than left as a stale row: {removed}"
    );
    let gone = refuse(
        &mut fixture.session,
        "dependents",
        json!({ "target": "decorate", "depth": 1 }),
    );
    assert_eq!(
        gone.outcome,
        Outcome::UnknownTarget,
        "an index that still answered for a deleted function would be reporting fiction: {}",
        gone.verdict_reason
    );
}

#[test]
fn refresh_mode_without_paths_is_refused_rather_than_walking_the_repository() {
    let mut fixture = indexed("refresh-no-paths");
    let error = refuse(&mut fixture.session, "index", json!({ "mode": "refresh" }));
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error.verdict_reason.contains("paths"),
        "the refusal names the argument that is missing: {}",
        error.verdict_reason
    );
    assert!(
        error.advice.as_deref().is_some_and(|a| a.contains("full")),
        "the advice names the mode that would do what was wanted: {:?}",
        error.advice
    );
}

#[test]
fn full_mode_with_paths_is_refused_rather_than_ignoring_them() {
    // The predecessor's documented behaviour was to accept a parameter that made no sense in the
    // position it was given and ignore it. Silently walking the whole repository when the caller
    // named one file is the worst version of that, because it is slow as well as wrong.
    let mut fixture = indexed("full-with-paths");
    let error = refuse(
        &mut fixture.session,
        "index",
        json!({ "mode": "full", "paths": ["src/ui.rs"] }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error.verdict_reason.contains("ignored"),
        "the refusal says what would have happened: {}",
        error.verdict_reason
    );
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains("refresh")),
        "the advice names the mode that would do what was wanted: {:?}",
        error.advice
    );
}

#[test]
fn an_unknown_mode_is_refused_with_both_modes_named() {
    let mut fixture = indexed("bad-mode");
    let error = refuse(&mut fixture.session, "index", json!({ "mode": "sideways" }));
    assert_eq!(error.outcome, Outcome::Refused);
    let advice = error.advice.unwrap_or_default();
    assert!(
        advice.contains("full") && advice.contains("refresh"),
        "a refusal that does not say what the right values are makes the caller guess again: {advice}"
    );
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[test]
fn doctor_runs_every_check_and_reports_the_measurement_behind_each_one() {
    let mut fixture = indexed("doctor-healthy");
    let diagnosis = call(&mut fixture.session, "doctor", json!({}));
    let findings = diagnosis["findings"]
        .as_array()
        .expect("an array of findings");
    assert!(
        findings.len() >= 8,
        "doctor has twelve checks and must run them rather than skip the ones it dislikes: \
         {diagnosis}"
    );
    for finding in findings {
        let check = finding["check"].as_str().expect("a named check");
        let severity = finding["severity"].as_str().expect("a severity");
        assert!(
            ["pass", "notice", "warn", "fail"].contains(&severity),
            "`{check}` has a severity this build does not define: {severity}"
        );
        assert!(
            finding["detail"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "`{check}` must state the measurement it was derived from even on a pass: {finding}"
        );
        assert!(
            finding["action"].is_string() || finding["action"].is_null(),
            "`{check}` has an action field, null when there is nothing to suggest: {finding}"
        );
    }
    assert_eq!(
        diagnosis["not_performed"],
        json!([]),
        "a check that could not be run is a finding, never an omission from this list"
    );
    assert!(
        diagnosis["stats"].is_object(),
        "a sound index can be measured: {diagnosis}"
    );
}

#[test]
fn doctor_on_a_repository_with_no_index_reports_an_empty_index() {
    // `Store::open` creates a missing index, so `doctor` on a never-indexed repository creates an
    // empty one and then diagnoses it. That is the engine's behaviour and the response states the
    // result rather than hiding it: the counts are zero and the checks ran.
    let mut fixture = unindexed("doctor-unindexed");
    let diagnosis = call(&mut fixture.session, "doctor", json!({}));
    assert!(
        diagnosis["stats"]["entity_count"].as_u64() == Some(0),
        "an index created for a diagnosis holds nothing: {diagnosis}"
    );
    assert_eq!(
        diagnosis["counts"]["fail"].as_u64(),
        Some(0),
        "an empty index is not a broken one, and the counts say so: {diagnosis}"
    );
    assert!(
        diagnosis["counts"]["pass"].as_u64().is_some_and(|n| n > 0),
        "and the checks that ran report that they ran: {diagnosis}"
    );
}

#[test]
fn doctor_serialises_its_vocabulary_as_the_words_the_engine_prints() {
    // The engine's `Severity` and `Check` are serialised by derives in `peek-core`, which have to
    // agree with the `as_str` the terminal report uses. This is what stops the JSON and the report
    // drifting into two vocabularies.
    let mut fixture = indexed("doctor-vocabulary");
    let first = call(&mut fixture.session, "doctor", json!({}));
    let second = call(&mut fixture.session, "doctor", json!({}));
    assert_eq!(
        first, second,
        "two diagnoses of an unchanged index are the same document"
    );

    let mut seen: Vec<&str> = Vec::new();
    for finding in first["findings"].as_array().expect("an array") {
        let check = finding["check"].as_str().expect("a named check");
        let severity = finding["severity"].as_str().expect("a severity");
        assert!(
            !check.is_empty() && check.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "`{check}` is a machine-readable name, and this is what one looks like"
        );
        assert!(
            ["pass", "notice", "warn", "fail"].contains(&severity),
            "`{check}` has a severity this build does not define: {severity}"
        );
        if !seen.contains(&check) {
            seen.push(check);
        }
    }
    for expected in [
        "index_openable",
        "integrity",
        "schema_version",
        "durability",
    ] {
        assert!(
            seen.contains(&expected),
            "the `{expected}` check ran, and its name is the one the engine prints: {seen:?}"
        );
    }
    let worst = first["worst"].as_str().expect("a worst severity");
    assert!(
        first["counts"][worst]
            .as_u64()
            .is_some_and(|count| count > 0),
        "and the count for the worst severity is not zero, so it was actually observed: {first}"
    );
}

#[test]
fn doctor_refuses_an_argument_it_does_not_take() {
    let mut fixture = indexed("doctor-argument");
    let error = refuse(
        &mut fixture.session,
        "doctor",
        json!({ "target": "process" }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains("context")),
        "the advice names a tool that does take a target: {:?}",
        error.advice
    );
}

// ---------------------------------------------------------------------------
// Ambiguity and unknown targets, across every tool that takes a target
// ---------------------------------------------------------------------------

#[test]
fn an_ambiguous_target_returns_candidates_rather_than_picking_one() {
    let mut fixture = indexed("ambiguous-target");
    for name in ["explain", "callers", "callees", "dependents", "context"] {
        let mut arguments = json!({ "target": "render" });
        if name == "context" {
            arguments["budget_tokens"] = json!(100_000);
        }
        let error = refuse(&mut fixture.session, name, arguments);
        assert_eq!(
            error.outcome,
            Outcome::AmbiguousTarget,
            "`{name}` must not pick one of two declarations called `render` (D-0004)"
        );
        let candidates = &error.candidates;
        assert_eq!(
            candidates.len(),
            2,
            "`{name}` must return every candidate, not one: {candidates:?}"
        );
        for candidate in candidates {
            assert!(
                candidate.path.ends_with(".rs"),
                "a candidate says which file it is in, or the caller cannot choose: {candidate:?}"
            );
            assert!(
                candidate.start_line.is_some_and(|line| line > 0),
                "a candidate says where to open it: {candidate:?}"
            );
            // The identity is the one field the caller has to hand back, so it is read off the wire
            // rather than off the struct: what crosses the boundary is the whole identity, not a
            // rendering of it that the caller would have to parse.
            let carried = serde_json::to_value(candidate).unwrap_or_else(|error| {
                panic!("a candidate is serialisable: {error}: {candidate:?}")
            });
            assert!(
                carried["id"].is_object(),
                "the identity is carried so the caller can pass it straight back: {candidate:?}"
            );
            assert!(
                !candidate.kind.is_empty(),
                "and what kind of declaration it is: {candidate:?}"
            );
        }
        assert!(
            error
                .advice
                .as_deref()
                .is_some_and(|a| a.contains("target")),
            "`{name}` says how to ask again: {:?}",
            error.advice
        );
    }
}

#[test]
fn an_unknown_target_names_which_of_the_three_lookups_missed() {
    let mut fixture = indexed("unknown-target");
    for name in ["explain", "callers", "callees", "dependents"] {
        let error = refuse(
            &mut fixture.session,
            name,
            json!({ "target": "no_such_symbol" }),
        );
        assert_eq!(
            error.outcome,
            Outcome::UnknownTarget,
            "`{name}` must distinguish 'no such symbol' from 'no such edges'"
        );
        assert!(
            error.candidates.is_empty(),
            "an unknown target has no candidates to offer: {:?}",
            error.candidates
        );
    }
    // A path-shaped target is the case where the detail has to say which lookup it nearly matched.
    let error = refuse(
        &mut fixture.session,
        "explain",
        json!({ "target": "src/nothing/here.rs" }),
    );
    assert_eq!(error.outcome, Outcome::UnknownTarget);
    assert!(
        error.verdict_reason.contains("repository path"),
        "a path-shaped target is told it read as a path and that no indexed file declares it: {}",
        error.verdict_reason
    );
}

#[test]
fn an_ambiguous_edge_reaches_the_caller_with_its_candidate_list() {
    // D-0009: an ambiguous edge presented as fact is worse than no edge, because the consumer
    // cannot tell a guess from a proof.
    let mut fixture = indexed("ambiguous-edge");
    let explained = call(
        &mut fixture.session,
        "explain",
        json!({ "target": "process" }),
    );
    let ambiguous = explained["uncertain"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|edge| edge["relation"]["resolution"]["state"] == json!("ambiguous"))
        .unwrap_or_else(|| {
            panic!("the planted ambiguity must survive into the answer: {explained}")
        });
    let candidates = ambiguous["relation"]["resolution"]["candidates"]
        .as_array()
        .expect("an ambiguous edge carries its candidates in the relation itself");
    assert_eq!(
        candidates.len(),
        2,
        "both candidates are present: {ambiguous}"
    );
    assert!(
        ambiguous["state"]
            .as_str()
            .is_some_and(|text| text.contains("ambiguous")),
        "the rendered state says so in words too: {ambiguous}"
    );
    let edge_count = explained["edges"].as_array().expect("an array").len() as u64;
    assert!(
        explained["decided_edges"].as_u64().expect("a count") <= edge_count,
        "the decided count cannot exceed the edge count: {explained}"
    );
    assert!(
        explained["uncertain"].as_array().map(Vec::len).unwrap_or(0) > 0,
        "the undecided edges are listed on their own: {explained}"
    );
}

#[test]
fn an_unresolved_edge_reaches_the_caller_with_its_reason() {
    let mut fixture = indexed("unresolved-edge");
    let explained = call(
        &mut fixture.session,
        "explain",
        json!({ "target": "process" }),
    );
    let unresolved = explained["uncertain"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|edge| edge["relation"]["resolution"]["state"] == json!("unresolved"))
        .unwrap_or_else(|| panic!("the planted unresolved edge must be reported: {explained}"));
    assert_eq!(
        unresolved["relation"]["resolution"]["reason"],
        json!("external"),
        "an unresolved edge says why it could not be placed: {unresolved}"
    );
    assert_eq!(
        unresolved["evidence_class"],
        json!(null),
        "and carries no evidence, because there is none for a target that was never established"
    );
    assert!(
        unresolved["relation"]["target_name"]
            .as_str()
            .is_some_and(|name| name.contains("read_to_string")),
        "while the name as written survives, which is what a reader needs to find it: {unresolved}"
    );
}

// ---------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------

#[test]
fn explain_lists_every_edge_at_the_target_with_its_state_and_span() {
    let mut fixture = indexed("explain-edges");
    let explained = call(
        &mut fixture.session,
        "explain",
        json!({ "target": "process" }),
    );
    let edges = explained["edges"].as_array().expect("an array");
    assert!(
        !edges.is_empty(),
        "the fixture's target has edges: {explained}"
    );
    for edge in edges {
        assert!(
            edge["state"].as_str().is_some_and(|text| !text.is_empty()),
            "every edge states how decided it is: {edge}"
        );
        assert!(
            edge["relation"]["span"]["start_line"].as_u64().is_some(),
            "every edge carries the span it was found at: {edge}"
        );
        assert!(
            ["outgoing", "incoming"].contains(&edge["side"].as_str().unwrap_or("")),
            "an edge says which side of the subject it is on: {edge}"
        );
        assert!(
            edge["relation"]["resolution"]["state"].is_string(),
            "the resolution state is the typed value from the index, not a rendering: {edge}"
        );
    }
    assert!(
        explained["entity"].is_object(),
        "the subject's identity is carried: {explained}"
    );
    assert!(
        explained["chain"].is_array(),
        "the chain is present even when it is empty: {explained}"
    );
    assert!(
        explained["chain_depth"].as_u64().is_some(),
        "the depth the chain was allowed is reported whether or not it used it: {explained}"
    );
    assert!(
        explained["options"].is_object(),
        "the options in force travel with the answer: {explained}"
    );
}

#[test]
fn explain_carries_the_evidence_of_a_proven_edge_and_no_invented_basis() {
    // D-0003: a call resolved by an import binding and one resolved by a globally unique name must
    // be distinguishable. The predecessor's constant `Edge.reason` made them byte-identical.
    let mut fixture = indexed("explain-evidence");
    let explained = call(
        &mut fixture.session,
        "explain",
        json!({ "target": "handler" }),
    );
    let proven = explained["edges"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|edge| edge["evidence_class"] == json!("import_binding"))
        .unwrap_or_else(|| {
            panic!("a call through a `use` statement is proven by the import binding: {explained}")
        });
    assert_eq!(
        proven["basis"],
        json!(null),
        "a proven edge carries no basis by design: the evidence class is the whole argument, and a \
         synthesised sentence here is the defect D-0003 removed"
    );
}

#[test]
fn explain_refuses_a_depth_argument_and_names_the_tool_that_takes_one() {
    let mut fixture = indexed("explain-depth");
    let error = refuse(
        &mut fixture.session,
        "explain",
        json!({ "target": "process", "depth": 3 }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    let advice = error.advice.unwrap_or_default();
    assert!(
        advice.contains("dependents") && advice.contains("chain_depth"),
        "the refusal says both what this tool calls it and which tool takes a depth: {advice}"
    );
}

#[test]
fn explain_refuses_a_budget_and_names_the_tool_that_takes_one() {
    let mut fixture = indexed("explain-budget");
    let error = refuse(
        &mut fixture.session,
        "explain",
        json!({ "target": "process", "budget_tokens": 4000 }),
    );
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains("context")),
        "an unbounded answer is not what a budgeted answer is for: {:?}",
        error.advice
    );
}

// ---------------------------------------------------------------------------
// The walks
// ---------------------------------------------------------------------------

#[test]
fn callers_returns_what_depends_on_the_target_at_distance_one() {
    let mut fixture = indexed("callers-one-hop");
    let walk = call(
        &mut fixture.session,
        "callers",
        json!({ "target": "settle" }),
    );
    assert_eq!(walk["outcome"], json!("ok"));
    let steps = walk["steps"].as_array().expect("an array");
    assert!(
        !steps.is_empty(),
        "the fixture's `settle` has callers: {walk}"
    );
    for step in steps {
        assert_eq!(
            step["distance"].as_u64(),
            Some(1),
            "`callers` is one hop by definition: {step}"
        );
    }
    assert_eq!(
        walk["request"]["depth"],
        json!(1),
        "the request is echoed: {walk}"
    );
    assert_eq!(walk["request"]["direction"], json!("inbound"));
    assert!(
        steps
            .iter()
            .any(|step| step["id"]["qualified_name"] == json!("process")),
        "the fixture has `process` calling `settle`: {steps:?}"
    );
    assert_eq!(
        walk["followed_inferred"].as_u64(),
        Some(
            steps
                .iter()
                .filter(|step| step["inferred"] == json!(true))
                .count() as u64
        ),
        "the count of inferred arrivals and the list of them are the same number"
    );
}

#[test]
fn callees_returns_what_the_target_uses() {
    let mut fixture = indexed("callees-one-hop");
    let walk = call(
        &mut fixture.session,
        "callees",
        json!({ "target": "process" }),
    );
    let steps = walk["steps"].as_array().expect("an array");
    assert!(
        steps
            .iter()
            .any(|step| step["id"]["qualified_name"] == json!("settle")),
        "the fixture has `process` calling `settle`: {steps:?}"
    );
    assert_eq!(walk["request"]["direction"], json!("outbound"));
}

#[test]
fn dependents_reaches_further_than_callers_and_reports_the_distance() {
    let mut fixture = indexed("dependents-depth");
    let one = call(
        &mut fixture.session,
        "dependents",
        json!({ "target": "reconcile", "depth": 1 }),
    );
    let two = call(
        &mut fixture.session,
        "dependents",
        json!({ "target": "reconcile", "depth": 2 }),
    );
    let one_steps = one["steps"].as_array().expect("an array").len();
    let two_steps = two["steps"].as_array().expect("an array").len();
    assert!(
        two_steps > one_steps,
        "a wider walk reaches further: {one_steps} at depth 1, {two_steps} at depth 2"
    );
    assert!(
        two["steps"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|step| step["distance"].as_u64() == Some(2)),
        "the second hop is labelled as such: {two}"
    );
    assert!(
        !two["steps"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|step| step["id"] == two["target"]),
        "the target is never one of its own dependents (D-0007): {two}"
    );
}

#[test]
fn dependents_at_depth_zero_returns_nothing_and_still_names_the_target() {
    let mut fixture = indexed("dependents-zero");
    let walk = call(
        &mut fixture.session,
        "dependents",
        json!({ "target": "settle", "depth": 0 }),
    );
    assert_eq!(walk["outcome"], json!("ok"));
    assert_eq!(
        walk["steps"].as_array().map(Vec::len),
        Some(0),
        "zero hops is a coherent answer, not a failure: {walk}"
    );
    assert!(
        walk["target"].is_object(),
        "the target is still named, so an empty list is distinguishable from an unresolved one: {walk}"
    );
    assert_eq!(
        walk["bounded"],
        json!(false),
        "zero hops is the question asked, not a bound: {walk}"
    );
}

#[test]
fn a_walk_reports_whether_a_limit_stopped_it() {
    // The difference between "nothing more depends on this" and "the walk ran out of budget" is
    // the difference between an answer and a shrug.
    let mut fixture = indexed("walk-bounded");
    let walk = call(
        &mut fixture.session,
        "dependents",
        json!({ "target": "settle", "depth": 3 }),
    );
    assert_eq!(
        walk["bounded"],
        json!(false),
        "the fixture's neighbourhood is small enough that nothing bounded it: {walk}"
    );
    assert!(
        walk["inspected"].as_u64().is_some_and(|n| n > 0),
        "the work done is reported: {walk}"
    );
    assert!(
        walk["visited"].as_u64().is_some(),
        "the entities expanded are reported: {walk}"
    );
    assert!(
        walk["headline"]
            .as_str()
            .is_some_and(|text| text.contains("relation(s) read")),
        "and the headline says how much was read: {walk}"
    );
}

#[test]
fn a_walk_step_carries_the_whole_relation_it_arrived_on() {
    let mut fixture = indexed("walk-relation");
    let walk = call(
        &mut fixture.session,
        "callers",
        json!({ "target": "settle" }),
    );
    // A `use` statement is a real dependency and its source is the *file*, so this walk reaches
    // `src/service.rs` itself alongside the two functions that call `settle`. A file has no line:
    // the extractor gives the `File` entity no span, `query::traverse` says so where it builds the
    // fallback span, and `Candidate::start_line` documents the null for exactly this case. So the
    // line is required of every declaration that *has* one, and the file case is required to be
    // line-less and still actionable — which is a stronger claim than "every step has a line",
    // because it pins the difference instead of pretending it is not there.
    let mut located = 0_u64;
    let mut files = 0_u64;
    for step in walk["steps"].as_array().expect("an array") {
        let relation = &step["via"];
        for key in ["kind", "source", "target_name", "span", "resolution"] {
            assert!(
                !relation[key].is_null(),
                "`{key}` survives the crossing from the index to the response: {step}"
            );
        }
        assert!(
            step["declaration"]["path"]
                .as_str()
                .is_some_and(|p| p.ends_with(".rs")),
            "a step says which file to open, or an identity is not an answer: {step}"
        );
        assert!(
            step["declaration"]["qualified_name"]
                .as_str()
                .is_some_and(|n| !n.is_empty()),
            "a step says what it is: {step}"
        );
        if step["declaration"]["kind"] == json!("file") {
            files += 1;
            assert!(
                step["declaration"]["start_line"].is_null(),
                "a file is not at a line, and inventing one is the thing this crate exists to stop \
                 doing: {step}"
            );
        } else {
            located += 1;
            assert!(
                step["declaration"]["start_line"].as_u64().is_some(),
                "a declaration says which line: {step}"
            );
        }
    }
    assert!(
        located > 0 && files > 0,
        "the fixture must reach both a declaration and the file that imports it, or the two cases \
         above are not being tested: {walk}"
    );
}

#[test]
fn a_walk_can_be_restricted_to_one_relation_kind_and_a_bad_kind_lists_the_ones_that_exist() {
    let mut fixture = indexed("walk-kind");
    let filtered = call(
        &mut fixture.session,
        "callers",
        json!({ "target": "settle", "kind": "calls" }),
    );
    assert_eq!(filtered["request"]["kind"], json!("calls"));
    assert!(
        filtered["steps"]
            .as_array()
            .expect("an array")
            .iter()
            .all(|step| step["via"]["kind"] == json!("calls")),
        "the filter is applied, not merely recorded: {filtered}"
    );

    // The predecessor answered `--kind nonsense` with every kind, and the caller read it as a
    // filtered result. A kind this build does not have is refused with the list.
    let error = refuse(
        &mut fixture.session,
        "callers",
        json!({ "target": "settle", "kind": "nonsense" }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    let advice = error.advice.unwrap_or_default();
    assert!(
        advice.contains("calls") && advice.contains("imports"),
        "the refusal lists the vocabulary rather than only saying no: {advice}"
    );
}

#[test]
fn callers_and_callees_refuse_a_depth_and_name_dependents() {
    let mut fixture = indexed("walk-no-depth");
    for name in ["callers", "callees"] {
        let error = refuse(
            &mut fixture.session,
            name,
            json!({ "target": "settle", "depth": 2 }),
        );
        assert_eq!(error.outcome, Outcome::Refused);
        assert!(
            error
                .advice
                .as_deref()
                .is_some_and(|a| a.contains("dependents")),
            "`{name}` must say which tool takes a depth rather than silently ignoring one: {:?}",
            error.advice
        );
    }
}

#[test]
fn a_walk_can_be_told_not_to_follow_inferred_edges() {
    let mut fixture = indexed("walk-no-inferred");
    let with = call(
        &mut fixture.session,
        "callers",
        json!({ "target": "settle" }),
    );
    let without = call(
        &mut fixture.session,
        "callers",
        json!({ "target": "settle", "follow_inferred": false }),
    );
    assert_eq!(with["options"]["follow_inferred"], json!(true));
    assert_eq!(without["options"]["follow_inferred"], json!(false));
    assert!(
        without["steps"]
            .as_array()
            .expect("an array")
            .iter()
            .all(|step| step["inferred"] == json!(false)),
        "no arrival is by inference when inferred edges are not followed: {without}"
    );
}

// ---------------------------------------------------------------------------
// context: the budget contract
// ---------------------------------------------------------------------------

/// The smallest budget the compiler will accept, read from the engine.
fn floor(session: &mut Session) -> u64 {
    session
        .minimum_budget()
        .expect("an indexed repository has a floor")
}

/// A budget `multiple` times the floor.
fn budgeted(session: &mut Session, target: &str, multiple: u64) -> Value {
    let budget = floor(session).saturating_mul(multiple);
    call(
        session,
        "context",
        json!({ "target": target, "budget_tokens": budget }),
    )
}

#[test]
fn a_context_pack_costs_no_more_than_the_budget_at_every_budget_tried() {
    let mut fixture = indexed("context-within-budget");
    // Four budgets spanning the two interesting regimes: at the floor, between the target and its
    // neighbourhood, and far above the whole neighbourhood. Derived from the floor rather than
    // written down, so the test asserts a relationship rather than a token count.
    for multiple in [1_u64, 4, 40, 400] {
        let pack = budgeted(&mut fixture.session, "process", multiple);
        let report = &pack["pack"]["budget"];
        let requested = report["requested_tokens"].as_u64().expect("a count");
        let spent = report["spent_tokens"].as_u64().expect("a count");
        let expected = floor(&mut fixture.session) * multiple;
        assert_eq!(
            requested, expected,
            "the pack reports the budget it was given"
        );
        assert!(
            spent <= requested,
            "at a budget of {requested} the pack cost {spent}; the budget is a ceiling, not a target"
        );
        assert_eq!(
            report["remaining_tokens"].as_u64(),
            Some(requested.saturating_sub(spent)),
            "what is spent and what is left account for the whole budget: {report}"
        );
        assert!(
            pack["outcome"]
                .as_str()
                .is_some_and(|text| ["ok", "reduced", "insufficient"].contains(&text)),
            "the outcome is one of the three budget states: {pack}"
        );
        // The engine's own status and the outcome word are one fact said twice, so the word is
        // turned into the status it stands for. Both sides are options, so a pack reporting no
        // status at all fails here rather than quietly matching.
        let expected_status = match pack["outcome"].as_str() {
            Some("ok") => "complete",
            Some("reduced") => "reduced",
            _ => "insufficient",
        };
        assert_eq!(
            pack["pack"]["budget"]["status"].as_str(),
            Some(expected_status),
            "the outcome and the engine's own status are the same fact: {pack}"
        );
    }
}

#[test]
fn a_smaller_budget_yields_a_prefix_of_the_ranking_rather_than_a_truncated_pack() {
    // The contract: a reduced pack is a prefix of the full ranking. A unit is never partially
    // included and the fill never skips a high-ranked unit to make room for a low-ranked one, so
    // what a smaller budget removes is the *end* of the list, not the middle of it.
    let mut fixture = indexed("context-prefix");
    let roomy = budgeted(&mut fixture.session, "process", 400);
    let roomy_names = unit_names(&roomy);
    assert!(
        roomy_names.len() > 1,
        "a roomy budget returns the neighbourhood, not just the target: {roomy}"
    );

    let mut smaller = None;
    for multiple in [40_u64, 20, 10, 6, 4, 3, 2, 1] {
        let pack = budgeted(&mut fixture.session, "process", multiple);
        let names = unit_names(&pack);
        if names.len() < roomy_names.len() {
            assert!(
                roomy_names.starts_with(&names),
                "at {multiple} times the floor the pack holds {names:?}, which must be a prefix of \
                 {roomy_names:?}"
            );
            smaller = Some((multiple, names));
            break;
        }
    }
    assert!(
        smaller.is_some(),
        "some budget in the sweep must produce a smaller pack than {roomy_names:?}"
    );
}

#[test]
fn a_reduced_pack_names_what_it_dropped_and_what_it_would_have_cost() {
    let mut fixture = indexed("context-omissions");
    // Find a budget that reduces rather than completes, and among those one that dropped a whole
    // declaration rather than only an edge. Both are found rather than written down, because the
    // fixture's costs are not numbers this test may assume.
    let mut reduced_units = None;
    let mut reduced_anything = None;
    for multiple in [1_u64, 2, 3, 4, 6, 8, 12, 20, 40] {
        let pack = budgeted(&mut fixture.session, "process", multiple);
        if pack["outcome"] != json!("reduced") {
            continue;
        }
        if reduced_anything.is_none() {
            reduced_anything = Some(pack.clone());
        }
        let dropped_a_unit = pack["pack"]["omitted"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|omission| omission["what"] == json!("unit"));
        if dropped_a_unit {
            reduced_units = Some(pack);
            break;
        }
    }
    let pack = reduced_anything.expect(
        "some budget between the floor and a roomy one must reduce the pack rather than complete it",
    );
    let omissions = pack["pack"]["omitted"].as_array().expect("an array");
    assert!(
        !omissions.is_empty(),
        "a reduced pack has omissions: {pack}"
    );
    for omission in omissions {
        for key in ["subject", "what", "reason", "cost"] {
            assert!(
                !omission[key].is_null(),
                "an omission states its {key}: {omission}"
            );
        }
        assert!(
            ["budget_exhausted", "exceeds_budget", "edge_limit"]
                .contains(&omission["reason"].as_str().unwrap_or("")),
            "an omission says why in the engine's own vocabulary: {omission}"
        );
        assert!(
            omission["cost"]["tokens"].as_u64().is_some(),
            "an omission says what it would have cost, so 'it did not fit' is checkable: {omission}"
        );
    }
    assert!(
        pack["pack"]["report"]
            .as_str()
            .is_some_and(|report| report.contains("dropped")),
        "the rendered header says what was dropped: {pack}"
    );
    assert!(
        pack["pack"]["notes"].is_array(),
        "and the pack carries its notes: {pack}"
    );

    if let Some(pack) = reduced_units {
        assert!(
            pack["pack"]["notes"]
                .as_array()
                .expect("an array")
                .iter()
                .any(|note| note.as_str().is_some_and(|text| text.contains("prefix"))),
            "a pack that dropped a whole declaration says it is a prefix of the full ranking: {pack}"
        );
    }
}

#[test]
fn a_budget_below_the_floor_is_refused_with_the_minimum_rather_than_exceeded() {
    // D-0009: a pack whose own explanation does not fit is a pack whose silence is the answer.
    let mut fixture = indexed("context-below-floor");
    let floor = floor(&mut fixture.session);
    assert!(floor > 0, "the floor is a real number: {floor}");
    let error = refuse(
        &mut fixture.session,
        "context",
        json!({ "target": "process", "budget_tokens": floor - 1 }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert_eq!(
        error.minimum_tokens,
        Some(floor),
        "the refusal names the smallest budget that would have been accepted, as a number"
    );
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains(&floor.to_string())),
        "the advice quotes it too, for a caller that reads the sentence: {:?}",
        error.advice
    );
}

#[test]
fn a_budget_of_zero_is_refused_with_the_minimum() {
    let mut fixture = indexed("context-zero-budget");
    let floor = floor(&mut fixture.session);
    let error = refuse(
        &mut fixture.session,
        "context",
        json!({ "target": "process", "budget_tokens": 0 }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert_eq!(error.minimum_tokens, Some(floor));
}

#[test]
fn a_missing_budget_is_refused_rather_than_defaulted() {
    // A default is a number nobody chose. The tool description says the budget is required, and
    // this is the test that says so.
    let mut fixture = indexed("context-no-budget");
    let floor = floor(&mut fixture.session);
    let error = refuse(
        &mut fixture.session,
        "context",
        json!({ "target": "process" }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert_eq!(
        error.minimum_tokens,
        Some(floor),
        "the refusal says where to start: {floor}"
    );
    assert!(
        error.verdict_reason.contains("budget_tokens"),
        "the refusal names the argument: {}",
        error.verdict_reason
    );
}

#[test]
fn a_pack_too_small_for_the_target_itself_is_a_refusal_not_a_slice() {
    let mut fixture = indexed("context-insufficient");
    // A budget above the floor but below the target's own declaration: `BudgetStatus::Insufficient`
    // is the state where the target itself does not fit, and the engine returns it as `Ok`.
    let mut found = None;
    for multiple in [1_u64, 2, 3, 4] {
        let pack = budgeted(&mut fixture.session, "process", multiple);
        if pack["outcome"] == json!("insufficient") {
            found = Some(pack);
            break;
        }
    }
    let pack = found.expect("a budget just above the floor must be too small for the target");
    assert_eq!(
        pack["pack"]["units"].as_array().map(Vec::len),
        Some(0),
        "an insufficient pack contains no declaration: {pack}"
    );
    let omitted = pack["pack"]["omitted"].as_array().expect("an array");
    assert!(
        !omitted.is_empty(),
        "every candidate is accounted for even in a refusal: {pack}"
    );
    assert!(
        omitted
            .iter()
            .all(|omission| omission["reason"] == json!("exceeds_budget")),
        "a refusal says the units cost more than the whole budget: {omitted:?}"
    );
    assert!(
        pack["pack"]["budget"]["spent_tokens"]
            .as_u64()
            .is_some_and(|spent| spent
                <= pack["pack"]["budget"]["requested_tokens"]
                    .as_u64()
                    .unwrap_or(0)),
        "even a refusal costs no more than the budget it was given: {pack}"
    );
    assert!(
        pack["reason"]
            .as_str()
            .is_some_and(|text| text.contains("refusal")),
        "and it says that it is one: {pack}"
    );
}

#[test]
fn a_complete_pack_says_the_neighbourhood_was_exhausted() {
    let mut fixture = indexed("context-complete");
    let pack = budgeted(&mut fixture.session, "process", 400);
    assert_eq!(
        pack["outcome"],
        json!("ok"),
        "a roomy budget completes: {pack}"
    );
    assert_eq!(
        pack["pack"]["omitted"].as_array().map(Vec::len),
        Some(0),
        "and drops nothing: {pack}"
    );
    assert!(
        pack["pack"]["notes"]
            .as_array()
            .expect("an array")
            .iter()
            .any(|note| note.as_str().is_some_and(|text| text.contains("exhausted"))),
        "a caller who asked for more than the neighbourhood holds has to be told there was nothing \
         left worth adding: {pack}"
    );
}

#[test]
fn a_pack_carries_the_engines_own_budget_arithmetic_unchanged() {
    // The engine's own tests recount a pack from its parts. This asserts that what crossed the MCP
    // boundary is the same object rather than a re-typed summary of it.
    let mut fixture = indexed("context-arithmetic");
    let pack = budgeted(&mut fixture.session, "process", 400);
    let units = pack["pack"]["unit_cost"]["tokens"].as_u64();
    let edges = pack["pack"]["edge_cost"]["tokens"].as_u64();
    let spent = pack["pack"]["budget"]["spent_tokens"]
        .as_u64()
        .expect("a count");
    // The engine does not serialise those two helpers, so the sum is taken from the parts the pack
    // does carry — which is the point: a caller can add the pack up itself.
    let summed: u64 = pack["pack"]["units"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|unit| {
            unit["cost"]["tokens"].as_u64().unwrap_or(0)
                + unit["edges_cost"]["tokens"].as_u64().unwrap_or(0)
        })
        .sum();
    assert!(
        summed <= spent,
        "the units and their edges cost {summed}, which cannot exceed the {spent} the pack reports \
         for itself ({pack})"
    );
    let _ = (units, edges);
    assert!(
        pack["pack"]["budget"]["counter"]["chars_per_token"]
            .as_u64()
            .is_some(),
        "the counting rule travels with the pack, so the arithmetic can be reproduced: {pack}"
    );
    assert!(
        pack["rendered"]
            .as_str()
            .is_some_and(|rendered| rendered.contains("budget:")),
        "the rendered text opens with the budget header, so the two agree: {pack}"
    );
    assert!(
        pack["pack"]["target"]["query"].as_str() == Some("process"),
        "the pack names what it was asked about: {pack}"
    );
    for unit in pack["pack"]["units"].as_array().expect("an array") {
        assert!(
            unit["reason"]["reason"].as_str().is_some(),
            "every unit states why it is in the pack: {unit}"
        );
    }
}

#[test]
fn every_edge_in_a_pack_carries_its_resolution_state_and_its_candidates() {
    let mut fixture = indexed("context-edge-states");
    let pack = budgeted(&mut fixture.session, "process", 400);
    let mut edges = 0_u64;
    let mut ambiguous = 0_u64;
    for unit in pack["pack"]["units"].as_array().expect("an array") {
        for edge in unit["edges"].as_array().expect("an array") {
            edges += 1;
            let state = &edge["relation"]["resolution"]["state"];
            assert!(
                state.is_string(),
                "an edge without a state is an edge whose certainty is unknown: {edge}"
            );
            if state == &json!("ambiguous") {
                ambiguous += 1;
                let candidates = edge["relation"]["resolution"]["candidates"]
                    .as_array()
                    .expect("an ambiguous edge carries candidates in the relation");
                assert!(
                    !candidates.is_empty(),
                    "an ambiguous edge that names no candidate is a guess presented as a fact: {edge}"
                );
            }
        }
    }
    assert!(edges > 0, "the fixture's target has edges to carry: {pack}");
    assert!(
        ambiguous > 0,
        "and one of them is the planted ambiguity: {pack}"
    );
    assert!(
        pack["rendered"]
            .as_str()
            .is_some_and(|text| text.contains("ambiguous")),
        "the rendered form says the edge is ambiguous in words as well: {pack}"
    );
    assert!(
        pack["rendered"]
            .as_str()
            .is_some_and(|text| text.contains("between:")),
        "and names what it is ambiguous between: {pack}"
    );
    assert!(
        pack["uncertain_edges"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "and the pack counts the edges the engine could not decide: {pack}"
    );
}

#[test]
fn context_refuses_an_argument_it_does_not_take_and_names_the_right_tool() {
    let mut fixture = indexed("context-arguments");
    let error = refuse(
        &mut fixture.session,
        "context",
        json!({ "target": "process", "budget_tokens": 1000, "depth": 2 }),
    );
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains("dependents")),
        "a depth is a traversal parameter, not a context one: {:?}",
        error.advice
    );
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn two_identical_calls_against_an_unchanged_index_return_the_same_bytes() {
    // Determinism is already established for the index; this is the same claim at the MCP boundary,
    // where a hash map or an unordered iteration would break it. `index` and the two watch tools are
    // absent on purpose: they change the index, and `index` reports elapsed time.
    let mut fixture = indexed("determinism");
    for (name, arguments) in [
        ("index_status", json!({})),
        ("doctor", json!({})),
        ("explain", json!({ "target": "process" })),
        ("callers", json!({ "target": "settle" })),
        ("callees", json!({ "target": "process" })),
        ("dependents", json!({ "target": "settle", "depth": 2 })),
        (
            "context",
            json!({ "target": "process", "budget_tokens": 20_000 }),
        ),
    ] {
        let first = call(&mut fixture.session, name, arguments.clone());
        let second = call(&mut fixture.session, name, arguments);
        assert_eq!(
            first.to_string(),
            second.to_string(),
            "`{name}` returned different bytes for the same call against an unchanged index"
        );
    }
}

#[test]
fn the_same_refusal_against_an_unchanged_index_returns_the_same_bytes() {
    let mut fixture = indexed("determinism-refusal");
    for (name, arguments) in [
        ("explain", json!({ "target": "render" })),
        ("explain", json!({ "target": "no_such_symbol" })),
        ("callers", json!({ "target": "settle", "depth": 4 })),
    ] {
        let first = tools::refusal(name, &refuse(&mut fixture.session, name, arguments.clone()));
        let second = tools::refusal(name, &refuse(&mut fixture.session, name, arguments));
        assert_eq!(
            first.structured.to_string(),
            second.structured.to_string(),
            "`{name}` refused differently on two identical calls"
        );
        assert_eq!(first.text, second.text);
    }
}

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

#[test]
fn watch_start_reports_what_it_watches_and_how_to_stop_it_then_stops_cleanly() {
    let mut fixture = indexed("watch-start-stop");
    let (started, text) = call_both(
        &mut fixture.session,
        "watch_start",
        json!({ "quiet_for_ms": 50, "ready_timeout_ms": 10_000 }),
    );
    assert_eq!(started["outcome"], json!("ok"));
    let id = started["watch_id"].as_u64().expect("an id");
    assert!(
        started["root"]
            .as_str()
            .is_some_and(|root| !root.is_empty())
    );
    assert!(
        started["watching"]
            .as_array()
            .is_some_and(|languages| !languages.is_empty()),
        "the response says which languages it will re-index: {started}"
    );
    // The stop call is spelled out, so a caller that reads the response knows how to end it.
    assert_eq!(started["stop_with"]["tool"], json!("watch_stop"));
    assert_eq!(started["stop_with"]["arguments"]["watch_id"], json!(id));
    // The human form, which is the `ToolAnswer`'s text and not a member of the structured body. A
    // client that renders content and ignores `structuredContent` sees only this, so a stop call
    // that is discoverable only by parsing JSON is not discoverable at all on that client.
    assert!(
        text.contains("watch_stop"),
        "the text form says it too: {text}"
    );
    assert_eq!(started["state"]["running"], json!(true));

    let stopped = call(
        &mut fixture.session,
        "watch_stop",
        json!({ "watch_id": id }),
    );
    assert_eq!(stopped["outcome"], json!("ok"));
    assert_eq!(stopped["stopped_cleanly"], json!(true));
    assert_eq!(stopped["watch_id"], json!(id));
    assert_eq!(
        stopped["applied"].as_u64(),
        Some(0),
        "no file changed, so nothing was applied: {stopped}"
    );
    assert_eq!(stopped["failed"].as_u64(), Some(0));
    assert!(
        stopped["final_refresh"].is_null(),
        "and there is no final refresh to report: {stopped}"
    );
    assert!(
        fixture.log.mentions("watch"),
        "the watcher's lifecycle is logged to the diagnostic stream: {:?}",
        fixture.log.lines()
    );
}

#[test]
fn watch_start_refuses_on_a_repository_with_no_index() {
    let mut fixture = unindexed("watch-unindexed");
    let error = refuse(&mut fixture.session, "watch_start", json!({}));
    assert_eq!(
        error.outcome,
        Outcome::NotIndexed,
        "a watcher over an index that does not exist would build one from whatever changed next, \
         which is not what the caller asked for"
    );
    assert_eq!(error.advice.as_deref(), Some("run the `index` tool first"));
}

#[test]
fn a_second_watch_is_refused_with_the_id_of_the_running_one() {
    let mut fixture = indexed("watch-twice");
    let first = call(
        &mut fixture.session,
        "watch_start",
        json!({ "quiet_for_ms": 50 }),
    );
    let id = first["watch_id"].as_u64().expect("an id");
    let error = refuse(
        &mut fixture.session,
        "watch_start",
        json!({ "quiet_for_ms": 50 }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error.verdict_reason.contains(&id.to_string()),
        "the refusal names the watch that is holding the writer: {}",
        error.verdict_reason
    );
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains("watch_stop")),
        "the advice names the tool that frees it: {:?}",
        error.advice
    );
    call(
        &mut fixture.session,
        "watch_stop",
        json!({ "watch_id": id }),
    );
}

#[test]
fn a_write_is_refused_while_a_watch_holds_the_writer() {
    // SQLite admits one writer. A second one would either block for the busy timeout or fail, and
    // saying so up front names the cause rather than reporting a timeout.
    let mut fixture = indexed("watch-write");
    let started = call(
        &mut fixture.session,
        "watch_start",
        json!({ "quiet_for_ms": 50 }),
    );
    let id = started["watch_id"].as_u64().expect("an id");
    let error = refuse(&mut fixture.session, "index", json!({ "mode": "full" }));
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error.verdict_reason.contains("one writer"),
        "the refusal names the cause: {}",
        error.verdict_reason
    );
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains(&id.to_string())),
        "and the watch to stop first: {:?}",
        error.advice
    );
    call(
        &mut fixture.session,
        "watch_stop",
        json!({ "watch_id": id }),
    );
}

#[test]
fn watch_stop_without_a_watch_is_refused_rather_than_succeeding() {
    let mut fixture = indexed("watch-stop-none");
    let error = refuse(&mut fixture.session, "watch_stop", json!({}));
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains("watch_start")),
        "the refusal names the tool that would make the call meaningful: {:?}",
        error.advice
    );
}

#[test]
fn watch_stop_with_the_wrong_id_is_refused_with_the_running_one() {
    let mut fixture = indexed("watch-stop-wrong-id");
    let started = call(
        &mut fixture.session,
        "watch_start",
        json!({ "quiet_for_ms": 50 }),
    );
    let id = started["watch_id"].as_u64().expect("an id");
    let error = refuse(
        &mut fixture.session,
        "watch_stop",
        json!({ "watch_id": id + 41 }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains(&id.to_string())),
        "the refusal names the watch that is running: {:?}",
        error.advice
    );
    call(
        &mut fixture.session,
        "watch_stop",
        json!({ "watch_id": id }),
    );
}

#[test]
fn watch_stop_without_an_id_stops_the_running_watch() {
    let mut fixture = indexed("watch-stop-implicit");
    let started = call(
        &mut fixture.session,
        "watch_start",
        json!({ "quiet_for_ms": 50 }),
    );
    let id = started["watch_id"].as_u64().expect("an id");
    let stopped = call(&mut fixture.session, "watch_stop", json!({}));
    assert_eq!(
        stopped["watch_id"],
        json!(id),
        "the running one is stopped: {stopped}"
    );
    assert_eq!(stopped["stopped_cleanly"], json!(true));
}

#[test]
fn index_status_shows_that_a_watch_is_running() {
    let mut fixture = indexed("watch-status");
    let started = call(
        &mut fixture.session,
        "watch_start",
        json!({ "quiet_for_ms": 50 }),
    );
    let id = started["watch_id"].as_u64().expect("an id");
    let status = call(&mut fixture.session, "index_status", json!({}));
    assert_eq!(status["watch"]["id"], json!(id));
    assert_eq!(status["watch"]["running"], json!(true));
    assert_eq!(status["watch"]["applied"].as_u64(), Some(0));
    assert_eq!(status["watch"]["reports_superseded"].as_u64(), Some(0));
    assert!(
        status["watch"]["last_refresh"].is_null(),
        "nothing has been applied yet: {status}"
    );
    call(
        &mut fixture.session,
        "watch_stop",
        json!({ "watch_id": id }),
    );
    let after = call(&mut fixture.session, "index_status", json!({}));
    assert!(
        after["watch"].is_null(),
        "and a stopped watch is not reported as running: {after}"
    );
}

#[test]
fn watch_start_refuses_an_argument_it_does_not_take() {
    let mut fixture = indexed("watch-argument");
    let error = refuse(
        &mut fixture.session,
        "watch_start",
        json!({ "target": "process" }),
    );
    assert!(
        error.advice.as_deref().is_some_and(|a| a.contains("index")),
        "a target is not something a watcher takes: {:?}",
        error.advice
    );
}

#[test]
fn a_watch_keeps_the_index_current_as_a_file_changes() {
    // The one watch test that depends on a real filesystem event. It is last in this file for a
    // reason: it is the only one here that can be slow, and the only one that could be flaky on a
    // platform whose notifications do not work. It polls for the change with a deadline rather than
    // sleeping a fixed amount, so it is as fast as the platform allows and as slow as it must be.
    //
    // Two claims, not one, and which of them is asserted depends on what the watch did — which is
    // why the branch is written out rather than folded into a single "either way" assertion. A
    // tolerated answer has to be an answer: asking a helper that panics on a refusal cannot report
    // one, so the tolerance was a claim the test could not execute and did not, and every failure
    // it was written for arrived as a panic from the middle of it instead.
    let mut fixture = indexed("watch-refreshes");
    let opened = call(&mut fixture.session, "index_status", json!({}));
    assert_eq!(
        opened["handle_is_stale"],
        json!(false),
        "the handle starts out current: {opened}"
    );
    let started = call(
        &mut fixture.session,
        "watch_start",
        json!({ "quiet_for_ms": 50, "ready_timeout_ms": 10_000 }),
    );
    let id = started["watch_id"].as_u64().expect("an id");

    fixture.root.write(
        "src/ui.rs",
        "\
pub fn render() -> u32 {
    3
}

pub fn decorate() -> u32 {
    4
}
",
    );

    // Polled rather than slept, so the test is as fast as the platform allows and as slow as it
    // must be. The ceiling is two seconds, which is far more than a local filesystem needs to
    // deliver one event and far less than a test suite should spend waiting for one.
    let mut applied = 0;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let status = call(&mut fixture.session, "index_status", json!({}));
        applied = status["watch"]["applied"].as_u64().unwrap_or(0);
        if applied > 0 {
            // The commit happens before the counter moves, so a non-zero `applied` is a committed
            // refresh — and this is where the session's own reader has fallen behind.
            assert_eq!(
                status["handle_is_stale"],
                json!(true),
                "a watcher committed beside a reader this session opened earlier, so the handle is \
                 behind and the tool has to say so: {status}"
            );
            assert!(
                status["recorded_generation"].as_u64() > status["opened_at_generation"].as_u64(),
                "and it is the index that moved on, not the handle: {status}"
            );
            break;
        }
    }
    let stopped = call(
        &mut fixture.session,
        "watch_stop",
        json!({ "watch_id": id }),
    );
    assert_eq!(
        stopped["stopped_cleanly"],
        json!(true),
        "the watch stopped cleanly whether or not it saw the change ({applied} applied): {stopped}"
    );
    assert_eq!(
        stopped["failed"].as_u64(),
        Some(0),
        "and no refresh failed: {stopped}"
    );
    // Whatever the platform did with the notification, the claim that must hold is that a stopped
    // watch leaves a consistent index rather than a claim about how quickly it noticed — and the
    // two ways of not noticing are answered differently, because one of them is a bug.
    if applied > 0 {
        // A refresh reached the store, so this is the strong claim: the function the file now
        // declares is answerable, and by the identity that refresh wrote. A watcher that applies a
        // batch of paths it cannot place under its own root fails here rather than quietly, because
        // the entity it wrote has a repository path nothing else in the index has.
        let found = call(
            &mut fixture.session,
            "dependents",
            json!({ "target": "decorate", "depth": 1 }),
        );
        assert_eq!(
            found["outcome"],
            json!("ok"),
            "a refresh was applied, so what the file now declares is answerable: {found}"
        );
        assert_ne!(
            found["target"]["matched"],
            json!("path"),
            "and a bare function name resolves to a declaration, not to a file path: {found}"
        );
        // The identity fields sit directly on `target` — `kind`, `ordinal`, `path`,
        // `qualified_name`. An earlier version of this asserted `target.entity.id.path`, which this
        // shape does not have, so the lookup returned null and the assertion failed on a *refresh
        // that had demonstrably applied*. Reading a field that is absent proves nothing about the
        // file; reading the one that is present proves what it says.
        assert_eq!(
            found["target"]["path"],
            json!("src/ui.rs"),
            "whose repository-relative identity is the file that changed, and not a path some \
             refresh could not place under this root: {found}"
        );
    } else {
        // Nothing was applied, so this platform delivered no notification at all. That is what the
        // tolerance is for, and it proves less than the branch above, which says so here rather
        // than letting the assertion speak for it. What still has to hold is consistency: the index
        // is exactly what it was, it says so plainly for what it does not hold, and it answers the
        // moment the same change is handed to it directly.
        let withheld = refuse(
            &mut fixture.session,
            "dependents",
            json!({ "target": "decorate", "depth": 1 }),
        );
        assert_eq!(
            withheld.outcome,
            Outcome::UnknownTarget,
            "with no refresh applied, a target that was never indexed is a refusal that names \
             itself rather than an empty answer: {}",
            withheld.verdict_reason
        );
        assert!(
            withheld.verdict_reason.contains("decorate"),
            "and the refusal names what it could not find: {}",
            withheld.verdict_reason
        );
        assert!(
            withheld.candidates.is_empty(),
            "an unknown target has no candidates to offer: {:?}",
            withheld.candidates
        );
        let untouched = call(
            &mut fixture.session,
            "dependents",
            json!({ "target": "settle", "depth": 1 }),
        );
        assert_eq!(
            untouched["outcome"],
            json!("ok"),
            "the index still answers for what it held before the file changed, which is what \
             consistency means here: {untouched}"
        );
        // Measured rather than assumed: the same index, the same session and the same file answer
        // `decorate` as soon as the change is handed over directly, so what is missing here is the
        // notification and nothing else. Without this the branch would be asserting that the
        // platform is incapable of watching, which is not something it can observe.
        call(
            &mut fixture.session,
            "index",
            json!({ "mode": "refresh", "paths": ["src/ui.rs"] }),
        );
        let by_hand = call(
            &mut fixture.session,
            "dependents",
            json!({ "target": "decorate", "depth": 1 }),
        );
        assert_eq!(
            by_hand["outcome"],
            json!("ok"),
            "so the index was able to answer all along, and the watch is what delivered nothing: \
             {by_hand}"
        );
    }
}

// ---------------------------------------------------------------------------
// The parameters every tool shares
// ---------------------------------------------------------------------------

#[test]
fn every_tool_refuses_an_argument_it_does_not_have() {
    let mut fixture = indexed("unknown-argument");
    for name in peek_mcp::tool::names() {
        let error = refuse(&mut fixture.session, name, json!({ "nonsense": 1 }));
        assert_eq!(
            error.outcome,
            Outcome::Refused,
            "`{name}` must refuse an argument it does not have rather than ignore it"
        );
        assert!(
            !error.advice.as_deref().unwrap_or("").is_empty(),
            "`{name}` must say what it would have accepted: {error:?}"
        );
    }
}

#[test]
fn a_tool_refuses_arguments_that_are_not_an_object() {
    let mut fixture = indexed("arguments-not-object");
    let error = tools::dispatch(&mut fixture.session, "explain", Some(&json!([1, 2, 3])))
        .expect_err("a list of arguments is not a call");
    assert!(
        error.verdict_reason.contains("list"),
        "the refusal says what arrived: {}",
        error.verdict_reason
    );
    assert!(
        error.advice.as_deref().is_some_and(|a| a.contains("named")),
        "and says what a call looks like instead: {:?}",
        error.advice
    );
}

#[test]
fn a_missing_required_argument_names_itself_and_says_what_it_is_for() {
    let mut fixture = indexed("missing-argument");
    let error = refuse(&mut fixture.session, "explain", json!({}));
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error.verdict_reason.contains("target"),
        "the refusal names the argument: {}",
        error.verdict_reason
    );
    assert!(
        error
            .advice
            .as_deref()
            .is_some_and(|a| a.contains("qualified name")),
        "and says what shape of value it wants: {:?}",
        error.advice
    );
}

#[test]
fn an_argument_of_the_wrong_type_names_the_type_it_found() {
    let mut fixture = indexed("wrong-type");
    let error = refuse(
        &mut fixture.session,
        "context",
        json!({ "target": "process", "budget_tokens": "four thousand" }),
    );
    assert!(
        error.verdict_reason.contains("string"),
        "the refusal says what arrived rather than only what was wanted: {}",
        error.verdict_reason
    );
}

#[test]
fn a_budget_that_is_not_a_whole_number_is_refused_rather_than_rounded() {
    let mut fixture = indexed("budget-fraction");
    let error = refuse(
        &mut fixture.session,
        "context",
        json!({ "target": "process", "budget_tokens": 1.5 }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error.advice.as_deref().is_some_and(|a| a.contains("whole")),
        "a fractional budget has no meaning here and is not rounded into one: {:?}",
        error.advice
    );
}

#[test]
fn a_depth_too_large_for_a_u32_is_refused_with_the_number_it_was() {
    // A saturated depth is a walk of a different graph than the one asked for, which is precisely
    // the silently-wrong answer this crate refuses to give.
    let mut fixture = indexed("depth-too-large");
    let error = refuse(
        &mut fixture.session,
        "dependents",
        json!({ "target": "settle", "depth": 5_000_000_000_u64 }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error.verdict_reason.contains("5000000000"),
        "the refusal quotes the number rather than only saying it was too large: {}",
        error.verdict_reason
    );
}

#[test]
fn a_negative_number_is_refused_rather_than_wrapping() {
    let mut fixture = indexed("negative-depth");
    let error = refuse(
        &mut fixture.session,
        "dependents",
        json!({ "target": "settle", "depth": -1 }),
    );
    assert_eq!(error.outcome, Outcome::Refused);
    assert!(
        error.verdict_reason.contains("whole number"),
        "a negative depth is not a depth: {}",
        error.verdict_reason
    );
}

#[test]
fn a_refresh_paths_list_must_hold_strings() {
    let mut fixture = indexed("refresh-paths-type");
    let error = refuse(
        &mut fixture.session,
        "index",
        json!({ "mode": "refresh", "paths": [1, 2] }),
    );
    assert!(
        error.verdict_reason.contains("list of strings"),
        "the refusal says what it wanted and what arrived: {}",
        error.verdict_reason
    );
    assert!(
        error.advice.as_deref().is_some_and(|a| a.contains("src/")),
        "and shows the shape of a path: {:?}",
        error.advice
    );
}
