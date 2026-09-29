//! Behaviour tests for the query engine.
//!
//! Every test here goes through the public API and asserts a *relationship* between two answers
//! rather than a magic number. That is deliberate: a hard-coded token count in a test is a
//! measurement nobody took, and this project's whole argument is that unmeasured numbers are the
//! defect. Where a test needs a budget it derives one from
//! [`Query::minimum_budget`] or by scaling a generous budget, and where it asserts a count it
//! asserts a count the fixture determines.
//!
//! The fixture is a small payments service, written by hand rather than extracted, so that every
//! resolution state is placed deliberately:
//!
//! ```text
//!   handler ──calls──▶ process ──calls──▶ settle ──calls──▶ reconcile
//!                        │                   ▲  │                │
//!                     uses_type             │  └────────────────┘  (a two-node cycle)
//!                        ▼                   │
//!                  StripeGateway        audit ──calls──┘
//!
//!   process ──calls──▶ render        ambiguous between src/ui/a.rs and src/ui/b.rs
//!   settle  ──calls──▶ external_thing unresolved: lives outside this repository
//!   handler ──implements──▶ StripeGateway   inferred, with a basis
//! ```
//!
//! `settle`'s cycle through `reconcile` is the termination test; `render` is the ambiguity test;
//! `external_thing` is the "the engine says it does not know" test; `StripeGateway` is the
//! inference test.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::entity::{Entity, EntityId, EntityKind};
use crate::model::language::Language;
use crate::model::path::RepoPath;
use crate::model::relation::{Evidence, Relation, RelationKind, ResolutionState, UnresolvedReason};
use crate::model::span::Span;
use crate::query::{
    BudgetStatus, ContextPack, Cost, Direction, EdgeSide, InclusionReason, Matched, OmissionReason,
    Omitted, Query, QueryError, QueryOptions, Walk, WalkRequest,
};
use crate::store::{IndexUpdate, RepoId, Store};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A directory that removes itself, so a failing test leaves nothing behind.
struct TempDir(PathBuf);

static NEXT: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "peek-query-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn database(&self) -> PathBuf {
        self.0.join("index.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("valid path")
}

fn id(file: &str, kind: EntityKind, qualified_name: &str) -> EntityId {
    EntityId::new(path(file), kind, qualified_name, 0)
}

/// A declaration with everything the pack will print populated, so a pricing test cannot pass by
/// accident on a field that happens to be absent.
fn entity(file: &str, kind: EntityKind, qualified_name: &str, line: u32) -> Entity {
    let start = line * 100;
    Entity {
        id: id(file, kind, qualified_name),
        // The **short** declaration name, with the scope-qualified form only in the id. That is
        // what the extractor emits, and it is load-bearing rather than cosmetic: `name` is the
        // indexed column a bare-name lookup seeks, so writing the qualified name into both makes
        // `peek("append")` unable to find `AuditTrail.append` — a failure that exists in the
        // fixture and not in a real index, which is the worst kind of fixture bug, because the
        // test then looks like a product defect.
        name: qualified_name
            .rsplit("::")
            .next()
            .unwrap_or(qualified_name)
            .to_owned(),
        signature: Some(format!(
            "fn {qualified_name}(request: &Request) -> Result<Receipt>"
        )),
        doc: Some(format!(
            "Handles {qualified_name} for the payments service."
        )),
        span: Span::new(start, start + 60, line, 1, line, 40),
        language: Some(Language::Rust),
        is_test: false,
        structural_fingerprint: None,
    }
}

/// A `File` entity, named the way the extractor names one: its own file name.
fn file_entity(file: &str) -> Entity {
    let name = path(file).file_name().to_owned();
    Entity {
        id: id(file, EntityKind::File, &name),
        name: name.clone(),
        signature: None,
        doc: None,
        span: None,
        language: Some(Language::Rust),
        is_test: false,
        structural_fingerprint: None,
    }
}

fn span(byte: u32) -> Span {
    Span::new(byte, byte + 12, byte / 40 + 1, 5, byte / 40 + 1, 17).expect("valid span")
}

const HANDLER: &str = "src/api/handler.rs";
const SERVICE: &str = "src/payments/service.rs";
const LEDGER: &str = "src/payments/ledger.rs";
const UI_A: &str = "src/ui/a.rs";
const UI_B: &str = "src/ui/b.rs";

fn handler_id() -> EntityId {
    id(HANDLER, EntityKind::Function, "handler")
}
fn process_id() -> EntityId {
    id(SERVICE, EntityKind::Function, "process")
}
fn settle_id() -> EntityId {
    id(LEDGER, EntityKind::Function, "settle")
}
fn reconcile_id() -> EntityId {
    id(LEDGER, EntityKind::Function, "reconcile")
}
fn audit_id() -> EntityId {
    id(LEDGER, EntityKind::Function, "audit")
}
fn gateway_id() -> EntityId {
    id(SERVICE, EntityKind::Struct, "StripeGateway")
}

/// A member, so its qualified name differs from its bare name and the bare-name lookup is
/// reachable at all. See `a_bare_name_resolves_to_a_declaration_and_arrives_with_its_container`.
fn audit_trail_id() -> EntityId {
    id(SERVICE, EntityKind::Struct, "AuditTrail")
}
fn append_id() -> EntityId {
    id(SERVICE, EntityKind::Method, "AuditTrail.append")
}
fn render_a() -> EntityId {
    id(UI_A, EntityKind::Function, "render")
}
fn render_b() -> EntityId {
    id(UI_B, EntityKind::Function, "render")
}
fn ledger_file() -> EntityId {
    id(LEDGER, EntityKind::File, "ledger.rs")
}

fn import(module: &str) -> Evidence {
    Evidence::ImportBinding {
        module: module.to_owned(),
        alias: None,
    }
}

/// The fixture graph, as data. Written out so a reader can check a claim about it without reading
/// the test that makes the claim.
///
/// The store is returned alongside the directory that holds it, so the file lives as long as the
/// handle does.
fn fixture() -> (Store, TempDir) {
    let dir = TempDir::new("fixture");
    let repo = RepoId::discover(dir.path()).expect("derive a repository id");
    let mut store = Store::open(&dir.database(), &repo).expect("open a fresh store");

    let mut update = IndexUpdate::empty();
    for entity in [
        file_entity(HANDLER),
        entity(HANDLER, EntityKind::Function, "handler", 10),
        file_entity(SERVICE),
        entity(SERVICE, EntityKind::Struct, "StripeGateway", 5),
        entity(SERVICE, EntityKind::Function, "process", 20),
        entity(SERVICE, EntityKind::Struct, "AuditTrail", 30),
        entity(SERVICE, EntityKind::Method, "AuditTrail.append", 35),
        file_entity(LEDGER),
        entity(LEDGER, EntityKind::Function, "settle", 10),
        entity(LEDGER, EntityKind::Function, "reconcile", 40),
        entity(LEDGER, EntityKind::Function, "audit", 60),
        file_entity(UI_A),
        entity(UI_A, EntityKind::Function, "render", 5),
        file_entity(UI_B),
        entity(UI_B, EntityKind::Function, "render", 5),
    ] {
        update = update.with_entity(entity);
    }

    let relations = vec![
        // The chain `handler → process → settle`, which is what depth 1 and depth 2 differ on.
        Relation::resolved(
            RelationKind::Calls,
            handler_id(),
            process_id(),
            "process",
            span(200),
            import("../payments/service"),
        ),
        Relation::resolved(
            RelationKind::Calls,
            process_id(),
            settle_id(),
            "settle",
            span(400),
            import("../payments/ledger"),
        ),
        // A second caller, so "how many depend on this" is not a one-element answer.
        Relation::resolved(
            RelationKind::Calls,
            audit_id(),
            settle_id(),
            "settle",
            span(600),
            Evidence::SameFile,
        ),
        // The cycle: `settle` calls `reconcile` and `reconcile` calls `settle`.
        Relation::resolved(
            RelationKind::Calls,
            settle_id(),
            reconcile_id(),
            "reconcile",
            span(800),
            Evidence::SameFile,
        ),
        Relation::resolved(
            RelationKind::Calls,
            reconcile_id(),
            settle_id(),
            "settle",
            span(840),
            Evidence::SameFile,
        ),
        // A second, weaker caller of `process`, so "which edge explains this arrival" has more
        // than one answer and the chain has something to choose between.
        Relation::resolved(
            RelationKind::Calls,
            audit_id(),
            process_id(),
            "process",
            span(620),
            Evidence::UniqueName,
        ),
        // The ambiguity: one name, two declarations, no decisive evidence.
        Relation::ambiguous(
            RelationKind::Calls,
            process_id(),
            "render",
            span(1000),
            vec![render_a(), render_b()],
        ),
        // Something the engine cannot place. Persisted and countable, never guessed.
        Relation::unresolved(
            RelationKind::Calls,
            settle_id(),
            "std::io::read_to_string",
            span(1200),
            UnresolvedReason::External,
        ),
        // A type the target mentions without calling.
        Relation::resolved(
            RelationKind::UsesType,
            process_id(),
            gateway_id(),
            "StripeGateway",
            span(1300),
            Evidence::SameFile,
        ),
        // Containment, which is the one structural edge the extractor really does emit, and the
        // only thing that gives a top-level member a container.
        Relation::resolved(
            RelationKind::Contains,
            audit_trail_id(),
            append_id(),
            "append",
            span(1500),
            Evidence::Containment,
        ),
        // The inference: one target, an auditable basis, and a state that says so.
        Relation::inferred(
            RelationKind::Implements,
            handler_id(),
            gateway_id(),
            "StripeGateway",
            span(1400),
            Evidence::QualifiedNameInScope {
                scope: SERVICE.to_owned(),
            },
            "the only StripeGateway declared in this crate",
        ),
    ];
    for relation in relations {
        update = update.with_relation(relation);
    }

    store.apply_update(update).expect("commit the fixture");
    (store, dir)
}

fn query(store: &Store) -> Query<'_> {
    Query::new(store)
}

/// A budget large enough that the fixture's whole neighbourhood fits, derived from the smallest
/// budget the compiler will accept rather than written down.
fn generous(query: &Query<'_>) -> u64 {
    query.minimum_budget().saturating_mul(16)
}

/// A budget that can afford the target's own block and nothing else.
///
/// Derived rather than written down, so the test asserts a relationship — "this pack is one unit
/// and everything else was dropped" — instead of a token count nobody measured. The reserve is
/// added because the report is charged first, and
/// `the_reserve_is_an_upper_bound_on_the_report` is what guarantees the report still fits in it.
fn exactly_the_target(query: &Query<'_>, roomy: &ContextPack) -> u64 {
    let target = roomy
        .units
        .first()
        .expect("a pack always contains its target unless it refused it");
    query
        .minimum_budget()
        .saturating_add(target.cost.tokens)
        .saturating_add(target.edges_cost.tokens)
}

/// Whether `pack` contains `id`. Written out because `Vec<&EntityId>::contains` wants a
/// `&&EntityId`, and every spelling of that in a test is a distraction from the claim.
fn includes(pack: &ContextPack, id: &EntityId) -> bool {
    pack.units.iter().any(|unit| &unit.entity.id == id)
}

// ---------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------

#[test]
fn a_resolved_edge_reports_the_evidence_the_resolver_recorded() {
    // D-0003's point: a call resolved by an import binding and one resolved by a globally unique
    // name must be distinguishable. Under Cortex's constant `Edge.reason` they were byte-identical.
    let (store, _dir) = fixture();
    let query = query(&store);
    let edge = store
        .outgoing(&process_id(), Some(RelationKind::Calls), 50)
        .expect("read the calls of process")
        .into_iter()
        .find(|relation| relation.target.as_ref() == Some(&settle_id()))
        .expect("process calls settle");

    let explained = query.explain_relation(&edge).expect("explain it");
    // The subject edge, plus every edge arriving at its source. The exact figure moves whenever
    // the fixture gains a caller, so the assertion is on the shape rather than on a hand-derived
    // number: the subject is first, it is outgoing, and the rest are the edges that explain why
    // the subject's source is interesting.
    assert!(
        explained.edges.len() >= 2,
        "the subject edge plus the edges arriving at its source: {explained:?}"
    );
    let answered = &explained.edges[0];
    assert_eq!(answered.side, EdgeSide::Outgoing);
    assert_eq!(answered.evidence_class.as_deref(), Some("import_binding"));
    assert_eq!(answered.state, "resolved (import_binding)");
    assert_eq!(
        answered.basis, None,
        "`Resolved` carries no basis by design: the evidence class is the whole argument, and \
         inventing a sentence here is the `Edge.reason` defect D-0003 removed"
    );
    assert_eq!(
        answered.relation.span, edge.span,
        "the span is the recorded span"
    );
}

#[test]
fn an_inferred_edge_reports_the_basis_the_resolver_wrote() {
    // The half of D-0003 the predecessor could not express at all: a claim rather than a proof,
    // with the claim available for audit.
    let (store, _dir) = fixture();
    let query = query(&store);
    let edge = store
        .outgoing(&handler_id(), None, 50)
        .expect("read the edges of handler")
        .into_iter()
        .find(|relation| relation.kind == RelationKind::Implements)
        .expect("handler implements StripeGateway");

    let explained = query.explain_relation(&edge).expect("explain it");
    let answered = &explained.edges[0];
    assert_eq!(
        answered.basis.as_deref(),
        Some("the only StripeGateway declared in this crate"),
        "the basis must be the resolver's own words, not a paraphrase"
    );
    assert_eq!(
        answered.evidence_class.as_deref(),
        Some("qualified_name_in_scope")
    );
    assert!(
        answered.state.contains("inferred"),
        "the state must say it is an inference: {}",
        answered.state
    );
}

#[test]
fn an_ambiguous_edge_reports_its_state_and_its_candidate_count() {
    // D-0004 and D-0009. The count is part of the state, so a consumer reading only the rendered
    // line still learns the edge is undecided and how undecided it is.
    let (store, _dir) = fixture();
    let query = query(&store);
    let edge = store
        .outgoing(&process_id(), Some(RelationKind::Calls), 50)
        .expect("read the calls of process")
        .into_iter()
        .find(|relation| relation.resolution.is_ambiguous())
        .expect("process has an ambiguous call");

    let explained = query.explain_relation(&edge).expect("explain it");
    let answered = &explained.edges[0];
    assert_eq!(answered.state, "ambiguous (2 candidates)");
    assert_eq!(answered.candidate_count(), 2);
    assert_eq!(
        answered.evidence_class, None,
        "an undecided edge has no evidence for a target that was never established"
    );
    assert!(
        answered.render().contains("ambiguous (2 candidates)"),
        "the rendered line must carry the ambiguity: {}",
        answered.render()
    );
}

#[test]
fn the_candidates_explain_reports_are_the_candidates_the_store_holds() {
    // The number `explain` prints and the list it hands back must come from one place. D-0004's
    // requirement met only on paper is a candidate list that is persisted and unreachable.
    let (store, _dir) = fixture();
    let query = query(&store);
    let edge = store
        .outgoing(&process_id(), Some(RelationKind::Calls), 50)
        .expect("read the calls of process")
        .into_iter()
        .find(|relation| relation.resolution.is_ambiguous())
        .expect("process has an ambiguous call");

    let explained = query.explain_relation(&edge).expect("explain it");
    let stored = store
        .ambiguous_candidates(&process_id(), RelationKind::Calls, "render")
        .expect("read the candidate rows");
    assert_eq!(explained.edges[0].candidates(), stored.as_slice());
    assert_eq!(
        explained.edges[0].candidate_count(),
        stored.len(),
        "the count `explain` prints must be the number of candidates the store holds"
    );
}

#[test]
fn an_unresolved_edge_is_reported_with_its_reason_rather_than_dropped() {
    // Audit B: the predecessor did `else { continue }` here, so a reference that resolved to
    // nothing left no trace. It has to be visible in an answer.
    let (store, _dir) = fixture();
    let query = query(&store);
    let explained = query.explain(&settle_id()).expect("explain settle");

    let unresolved: Vec<_> = explained
        .edges
        .iter()
        .filter(|edge| edge.relation.resolution.is_unresolved())
        .collect();
    assert_eq!(unresolved.len(), 1, "settle has one unresolved call");
    assert_eq!(unresolved[0].state, "unresolved (external)");
    assert!(
        unresolved[0].candidates().is_empty(),
        "an edge with no target has no candidates, and must not be handed a made-up one"
    );
}

#[test]
fn the_chain_takes_one_path_and_says_how_many_edges_it_passed_over() {
    // Three edges reach `settle`. The chain takes one, and the answer says that a choice was
    // made rather than implying the others do not exist.
    let (store, _dir) = fixture();
    let query = query(&store);
    let explained = query.explain(&settle_id()).expect("explain settle");

    assert!(
        explained.chain.len() < 3,
        "a chain is one path, not the whole inbound set: {chain:?}",
        chain = explained.chain
    );
    let first = &explained.chain[0];
    assert_eq!(
        first.id,
        process_id(),
        "the import binding is the strongest edge"
    );
    assert_eq!(
        first.alternatives, 3,
        "three edges reach settle; one was taken"
    );
    assert_eq!(first.distance, 1);
    assert!(
        first.chosen_because.contains("strongest"),
        "the rule must be disclosed: {}",
        first.chosen_because
    );
    assert!(
        first.chosen_because.contains("import_binding"),
        "{}",
        first.chosen_because
    );
}

#[test]
fn the_chain_continues_to_the_caller_of_the_caller_and_then_says_it_stopped() {
    let (store, _dir) = fixture();
    let query = query(&store);
    let explained = query.explain(&settle_id()).expect("explain settle");

    let second = &explained.chain[1];
    assert_eq!(second.distance, 2);
    assert_eq!(second.id, handler_id(), "process is called by handler");
    // `alternatives` counts the edges the chain passed over at this hop, and the chosen hop's own
    // basis names the same count — so the two must agree. Asserted on that relationship rather
    // than on a literal: the figure is a property of the fixture's edge count, and a test that
    // hard-codes it breaks every time a fixture gains an edge, which is a nuisance rather than a
    // finding.
    assert!(
        second.chosen_because.contains(&second.alternatives.to_string()),
        "the basis must report the count it passed over: {}, alternatives {}",
        second.chosen_because,
        second.alternatives
    );
    assert!(
        !second.is_inferred(),
        "the chosen hop is a proof, not an inference"
    );
    assert!(
        explained
            .notes
            .iter()
            .any(|note| note.contains("the chain ends here")),
        "the chain must say why it stopped: {notes:?}",
        notes = explained.notes
    );
}

#[test]
fn a_symbol_nobody_points_at_has_an_empty_chain_and_the_answer_says_so() {
    // A top-level function with no callers has no inbound edge at all, so the chain ends
    // immediately. That is a fact about the index, and the answer says so rather than returning
    // an empty list that reads like a bug.
    let (store, _dir) = fixture();
    let query = query(&store);
    let explained = query.explain(&audit_id()).expect("explain audit");

    assert!(explained.chain.is_empty(), "nothing points at audit");
    assert!(
        explained
            .notes
            .iter()
            .any(|note| note.contains("the chain ends here")),
        "the answer must say why the chain is empty: {notes:?}",
        notes = explained.notes
    );
    assert!(
        explained
            .notes
            .iter()
            .any(|note| note.contains("is a root")),
        "and that nothing points at it at all: {notes:?}",
        notes = explained.notes
    );
}

#[test]
fn explaining_an_entity_the_index_does_not_hold_is_a_named_failure() {
    // A query for a symbol that does not exist must not look like a broken index, and must not
    // look like a symbol with nothing to do with anything.
    let (store, _dir) = fixture();
    let query = query(&store);
    match query.explain(&id("src/nowhere.rs", EntityKind::Function, "ghost")) {
        Err(QueryError::NotIndexed { query }) => assert!(query.contains("ghost"), "{query}"),
        other => panic!("expected NotIndexed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

#[test]
fn one_hop_inbound_finds_the_direct_callers_and_nothing_further() {
    let (store, _dir) = fixture();
    let walk = query(&store)
        .dependents(&settle_id(), 1)
        .expect("walk from settle");

    let found: Vec<&str> = walk.steps.iter().map(|step| step.id.name()).collect();
    assert_eq!(
        found,
        vec!["audit", "reconcile", "process"],
        "ordered by identity, not by discovery: {walk:?}"
    );
    assert_eq!(walk.max_distance(), Some(1));
    assert!(walk.is_complete(), "one hop is not a bound: {walk:?}");
    assert!(
        !walk.contains(&settle_id()),
        "D-0007: `dependents` must not include its own target"
    );
    assert_eq!(
        walk.closed, 0,
        "the neighbourhood around `settle` at one hop is a tree: {walk:?}"
    );
    assert_eq!(walk.revisits, 0, "{walk:?}");
}

#[test]
fn two_hops_reach_further_than_one_and_depth_zero_reaches_nothing() {
    let (store, _dir) = fixture();
    let query = query(&store);

    let one = query.dependents(&settle_id(), 1).expect("one hop");
    let two = query.dependents(&settle_id(), 2).expect("two hops");
    let none = query.dependents(&settle_id(), 0).expect("no hops");

    assert!(two.steps.len() > one.steps.len(), "{one:?} vs {two:?}");
    assert!(
        two.contains(&handler_id()),
        "two hops must reach the caller of a caller: {two:?}"
    );
    assert_eq!(two.at_distance(2).count(), 1, "{two:?}");
    assert!(
        none.steps.is_empty(),
        "depth 0 asks for nothing beyond the target: {none:?}"
    );
    assert_eq!(none.target, settle_id(), "the target is still named");
}

#[test]
fn a_walk_terminates_on_a_cycle_and_says_it_found_one() {
    // `settle` and `reconcile` call each other. Termination must be by construction, and the
    // answer must be able to prove it rather than merely fail to hang.
    let (store, _dir) = fixture();
    let walk = query(&store)
        .dependents(&settle_id(), 4)
        .expect("walk from settle through the cycle");

    assert_eq!(
        walk.closed, 2,
        "two followable edges were declined because their other end was already reached: \
         `reconcile → settle` turns back to the target and closes the cycle, and `audit → process` \
         is a second path to an entity already found: {walk:?}"
    );
    assert_eq!(
        walk.closed - walk.revisits,
        1,
        "exactly one of them turned back to the target itself: {walk:?}"
    );
    assert_eq!(walk.revisits, 1, "`audit → process` only: {walk:?}");
    assert!(
        walk.max_distance().is_some_and(|distance| distance <= 4),
        "a walk cannot exceed the depth it was given: {walk:?}"
    );
    assert_eq!(
        walk.visited, 5,
        "settle, audit, process, reconcile, handler — each expanded once: {walk:?}"
    );
}

#[test]
fn a_walk_follows_an_inferred_edge_and_the_caller_can_tell() {
    // An `Inferred` edge has one target and a basis, so hiding it would discard auditable
    // information and do it silently. It is followed, marked, and switchable.
    let (store, _dir) = fixture();
    let query = query(&store);

    let followed = query.dependents(&gateway_id(), 1).expect("walk");
    assert!(followed.contains(&handler_id()), "{followed:?}");
    let step = followed
        .steps
        .iter()
        .find(|step| step.id == handler_id())
        .expect("handler implements the gateway");
    assert!(
        step.is_inferred(),
        "the step must say it arrived by inference"
    );
    assert_eq!(followed.followed_inferred, 1);
    assert!(
        step.state().contains("inferred"),
        "the state must be visible: {}",
        step.state()
    );

    let strict = Query::with_options(
        &store,
        QueryOptions {
            follow_inferred: false,
            ..QueryOptions::default()
        },
    )
    .dependents(&gateway_id(), 1)
    .expect("walk");
    // With the policy off, the *inference* is not followed. The walk is not necessarily empty:
    // the fixture also has a proven `UsesType` edge into the same target, and a proof is exactly
    // the kind of edge this switch is not meant to suppress. Asserting emptiness would be
    // asserting that the target had no proven dependents, which is a different claim and a
    // false one.
    assert!(
        !strict.steps.iter().any(|step| step.id == handler_id()),
        "with the policy off, an inference is not a dependency: {strict:?}"
    );
    assert_eq!(
        strict.followed_inferred, 0,
        "no inference may have been followed: {strict:?}"
    );
}

#[test]
fn a_walk_never_follows_an_edge_with_no_target() {
    // Following an edge with no target means picking one. That is D-0004.
    let (store, _dir) = fixture();
    let walk = query(&store)
        .callees(&process_id())
        .expect("walk from process");

    let found: Vec<&str> = walk.steps.iter().map(|step| step.id.name()).collect();
    assert_eq!(found, vec!["settle", "StripeGateway"], "{walk:?}");
    assert!(
        walk.inspected > walk.steps.len(),
        "the ambiguous `render` call was read and counted, then not followed: {walk:?}"
    );
}

#[test]
fn a_file_target_answers_with_the_declarations_it_contains() {
    // A file's dependents are not the things that name the file — almost nothing does — so a walk
    // from a file expands to its members first. That expansion arrives as an *inferred* edge,
    // because the index records no file-to-declaration relation.
    let (store, _dir) = fixture();
    let walk = query(&store)
        .dependents(&ledger_file(), 1)
        .expect("walk from the ledger file");

    let mut found: Vec<&str> = walk.steps.iter().map(|step| step.id.name()).collect();
    found.sort_unstable();
    assert_eq!(found, vec!["audit", "reconcile", "settle"], "{walk:?}");
    assert!(
        walk.seeds.len() >= 3,
        "the file expanded to its declarations: {seeds:?}",
        seeds = walk.seeds
    );
    let membership = walk
        .steps
        .iter()
        .find(|step| step.id == settle_id())
        .expect("settle is in the file");
    assert!(
        membership.is_inferred(),
        "membership the index does not record must be marked as derived: {}",
        membership.state()
    );
}

#[test]
fn a_kind_filter_narrows_the_walk_without_changing_what_a_step_is() {
    let (store, _dir) = fixture();
    let walk = query(&store)
        .walk(
            &settle_id(),
            WalkRequest::dependents(1).with_kind(Some(RelationKind::Calls)),
        )
        .expect("walk");
    assert!(
        walk.steps
            .iter()
            .all(|step| step.via.kind == RelationKind::Calls),
        "{walk:?}"
    );
    assert_eq!(walk.request.direction, Direction::Inbound);
}

// ---------------------------------------------------------------------------
// The context compiler: the budget
// ---------------------------------------------------------------------------

#[test]
fn a_budget_too_small_to_hold_the_report_is_refused_rather_than_exceeded() {
    // D-0009. A pack whose own explanation does not fit is a pack whose silence is the answer.
    let (store, _dir) = fixture();
    let query = query(&store);
    let minimum = query.minimum_budget();
    assert!(minimum > 0, "the empty report costs something");

    match query.peek("process", minimum - 1) {
        Err(QueryError::BudgetTooSmall {
            requested,
            minimum: told,
        }) => {
            assert_eq!(requested, minimum - 1);
            assert_eq!(told, minimum, "the error must state what would have worked");
        }
        other => panic!("expected BudgetTooSmall, got {other:?}"),
    }
}

#[test]
fn a_pack_never_costs_more_than_it_was_given_at_any_budget() {
    // Checked across a range rather than at one budget, because the reserve arithmetic is the
    // thing that could be wrong and one point would not find it.
    let (store, _dir) = fixture();
    let query = query(&store);
    let minimum = query.minimum_budget();

    for multiplier in [1_u64, 2, 3, 5, 8, 16] {
        let budget = minimum.saturating_mul(multiplier);
        let pack = query
            .peek("process", budget)
            .unwrap_or_else(|error| panic!("a budget of {budget} should be accepted: {error}"));
        assert!(
            pack.budget.within_budget(),
            "asked for {budget}, spent {}: {}",
            pack.budget.spent_tokens,
            pack.report
        );
        assert_eq!(pack.budget.requested_tokens, budget);
        assert_eq!(
            pack.budget.spent_tokens + pack.budget.remaining_tokens,
            budget,
            "spent and remaining must account for the whole budget"
        );
    }
}

#[test]
fn the_reported_cost_is_the_sum_of_the_costs_of_the_parts() {
    // The one arithmetic property everything else rests on: the header says what the pack costs,
    // and the pack's own parts add up to that number.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("process", generous(&query))
        .expect("compile the whole neighbourhood");

    let counter = pack.budget.counter;
    let recount = pack
        .units
        .iter()
        .map(|unit| {
            assert_eq!(
                unit.cost,
                Cost::of(&unit.render(), counter),
                "a unit's declared cost must be the cost of its own rendering: {}",
                unit.render()
            );
            unit.cost.tokens + unit.edges_cost.tokens
        })
        .sum::<u64>()
        + counter.count(&pack.report);

    assert_eq!(
        recount, pack.budget.spent_tokens,
        "the header claims {} but the parts add to {recount}",
        pack.budget.spent_tokens
    );
    assert!(
        pack.edge_cost().tokens > 0,
        "the fixture's target has edges, so the pack prices more than its declarations"
    );
}

// ---------------------------------------------------------------------------
// The context compiler: selection, dropping and uncertainty
// ---------------------------------------------------------------------------

#[test]
fn every_included_declaration_states_why_it_is_included() {
    // Selection that cannot be explained is a selection `explain` would contradict.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("process", generous(&query))
        .expect("compile the whole neighbourhood");

    assert!(
        pack.units.len() >= 4,
        "process, its file's neighbours, its callee, its type and its caller: {pack:?}"
    );
    assert_eq!(
        pack.units[0].reason,
        InclusionReason::Target,
        "the thing that was asked for comes first"
    );
    for unit in &pack.units {
        let reason = unit.reason.describe();
        assert!(
            !reason.is_empty(),
            "{} has no stated reason",
            unit.entity.id
        );
        assert!(
            unit.render().contains(&reason),
            "the rendered unit must carry its own reason: {}",
            unit.render()
        );
    }
    assert!(
        pack.units
            .iter()
            .any(|unit| matches!(unit.reason, InclusionReason::Callee { .. })),
        "process calls settle, so settle is a callee: {ids:?}",
        ids = pack.unit_ids()
    );
    assert!(
        pack.units
            .iter()
            .any(|unit| matches!(unit.reason, InclusionReason::Caller { .. })),
        "handler calls process, so handler is a caller: {ids:?}",
        ids = pack.unit_ids()
    );
    assert!(
        pack.units
            .iter()
            .any(|unit| matches!(unit.reason, InclusionReason::Referenced { .. })),
        "process mentions StripeGateway as a type: {ids:?}",
        ids = pack.unit_ids()
    );
}

#[test]
fn every_edge_in_the_pack_carries_its_resolution_state() {
    // D-0009: an edge presented without its ambiguity is worse than no edge, because the consumer
    // cannot tell a guess from a fact.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("process", generous(&query))
        .expect("compile the whole neighbourhood");

    let mut states: Vec<String> = Vec::new();
    for edge in pack.edges() {
        let state = edge.state();
        assert!(!state.is_empty(), "an edge with no state: {edge:?}");
        assert_eq!(
            edge.relation.resolution.describe(),
            state,
            "the printed state must be the typed one"
        );
        let rendered = edge.render(&pack.units[0].entity.id, true);
        assert!(
            rendered.contains(&format!("[{state}]")),
            "the rendered line must carry the state: {rendered}"
        );
        states.push(state);
    }

    assert!(
        states
            .iter()
            .any(|state| state.contains("ambiguous (2 candidates)")),
        "the ambiguous call must survive into the pack with its count: {states:?}"
    );
    assert!(
        states.iter().any(|state| state.contains("resolved")),
        "the proven calls must be there too: {states:?}"
    );
}

#[test]
fn an_ambiguous_edge_in_the_pack_names_its_candidates() {
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("process", generous(&query))
        .expect("compile the whole neighbourhood");

    let ambiguous = pack
        .edges()
        .find(|edge| edge.relation.resolution.is_ambiguous())
        .expect("the ambiguous call is in the pack");
    assert_eq!(ambiguous.candidates().len(), 2);
    let rendered = ambiguous.render(&pack.units[0].entity.id, true);
    assert!(
        rendered.contains("src/ui/a.rs::render") && rendered.contains("src/ui/b.rs::render"),
        "the reader must be able to see what the choice was between: {rendered}"
    );

    let counted_only = ambiguous.render(&pack.units[0].entity.id, false);
    assert!(
        counted_only.contains("2 candidate(s), not listed"),
        "with the option off the count survives and the list does not: {counted_only}"
    );
}

#[test]
fn an_unresolved_edge_stays_in_the_pack_with_its_reason() {
    // The compiler must not drop an edge for being unhelpful. `settle` has an unresolved call; it
    // belongs in an answer about `settle` as much as a resolved one does.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("settle", generous(&query))
        .expect("compile settle");

    let unresolved = pack
        .edges()
        .find(|edge| edge.relation.resolution.is_unresolved())
        .expect("the unresolved call is in the pack");
    assert_eq!(unresolved.state(), "unresolved (external)");
    assert!(
        unresolved.relation.target.is_none(),
        "nothing may be invented for an edge the index could not place"
    );
}

#[test]
fn a_small_budget_produces_a_smaller_pack_and_says_what_it_dropped() {
    // The acceptance gate: the same question with a smaller budget produces a *smaller* answer
    // with a stated omission list, not the same answer cut in half.
    let (store, _dir) = fixture();
    let query = query(&store);
    let roomy = query
        .peek("process", generous(&query))
        .expect("compile generously");
    assert!(
        roomy.budget.status == BudgetStatus::Complete,
        "the generous budget fits the whole neighbourhood: {}",
        roomy.report
    );

    let tight = query
        .peek("process", exactly_the_target(&query, &roomy))
        .expect("compile tightly");
    assert_eq!(tight.budget.status, BudgetStatus::Reduced);
    assert!(
        tight.units.len() < roomy.units.len(),
        "a smaller budget must produce a smaller answer: {} vs {}",
        tight.units.len(),
        roomy.units.len()
    );
    assert!(!tight.omitted.is_empty(), "dropping must be reported");
    assert!(
        tight.report.contains("dropped:"),
        "the report must carry the omissions: {}",
        tight.report
    );
    assert!(
        tight
            .notes
            .iter()
            .any(|note| note.contains("prefix of the full ranking")),
        "the fill's stopping rule must be stated: {notes:?}",
        notes = tight.notes
    );
}

#[test]
fn a_smaller_budget_produces_a_coherent_subset_rather_than_a_truncated_one() {
    // Every declaration in the small pack is present in the large one, and every one of them is
    // whole: no half a signature, no half a doc comment.
    let (store, _dir) = fixture();
    let query = query(&store);
    let roomy = query
        .peek("process", generous(&query))
        .expect("compile generously");
    let tight = query
        .peek("process", exactly_the_target(&query, &roomy))
        .expect("compile tightly");

    let roomy_ids = roomy.unit_ids();
    assert_eq!(
        tight.units.len(),
        1,
        "a budget that fits the target's own block fits one unit: {pack}",
        pack = tight.report
    );
    for unit in &tight.units {
        assert!(
            includes(&roomy, &unit.entity.id),
            "{} is in the small pack but not the large one",
            unit.entity.id
        );
        assert_eq!(
            unit.cost,
            Cost::of(&unit.render(), tight.budget.counter),
            "a unit in the small pack must be priced as what it renders"
        );
    }
    // A prefix, not an arbitrary subset: the fill stops rather than skipping ahead.
    let tight_ids = tight.unit_ids();
    assert_eq!(
        tight_ids,
        roomy_ids[..tight_ids.len()],
        "the small pack must be a prefix of the ranking, in the same order"
    );
    assert!(
        tight
            .omitted
            .iter()
            .all(|entry| entry.what == Omitted::Unit),
        "a small pack is short of declarations, not of edges: {omitted:?}",
        omitted = tight.omitted
    );
}

#[test]
fn a_budget_that_cannot_hold_the_target_reports_a_refusal_rather_than_a_slice() {
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("process", query.minimum_budget())
        .expect("a budget equal to the report is accepted, then refused the target");

    assert_eq!(pack.budget.status, BudgetStatus::Insufficient);
    assert!(
        pack.units.is_empty(),
        "there is no such thing as half a target"
    );
    assert!(
        pack.omitted
            .iter()
            .all(|entry| entry.what == Omitted::Unit
                && entry.reason == OmissionReason::ExceedsBudget),
        "every candidate is named as too large: {omitted:?}",
        omitted = pack.omitted
    );
    assert!(
        pack.notes
            .iter()
            .any(|note| note.contains("a refusal, not a slice")),
        "{notes:?}",
        notes = pack.notes
    );
}

#[test]
fn a_budget_far_larger_than_the_answer_says_the_answer_was_complete() {
    // "Do not quietly under-fill." A caller who asked for a great deal and received a little needs
    // to know the answer is complete rather than the compiler having stopped.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query.peek("process", 1_000_000).expect("compile");

    assert_eq!(pack.budget.status, BudgetStatus::Complete);
    assert!(
        pack.budget.remaining_tokens > 0,
        "the neighbourhood was exhausted, so tokens are left over: {:?}",
        pack.budget
    );
    assert!(
        pack.notes
            .iter()
            .any(|note| note.contains("nothing left worth adding")),
        "{notes:?}",
        notes = pack.notes
    );
}

// ---------------------------------------------------------------------------
// The context compiler: targets
// ---------------------------------------------------------------------------

#[test]
fn an_ambiguous_target_is_reported_with_its_candidates_rather_than_picked() {
    // D-0004, at the target boundary. `symbols.first()` is banned here too.
    let (store, _dir) = fixture();
    let query = query(&store);
    match query.peek("render", generous(&query)) {
        Err(QueryError::AmbiguousTarget {
            query: asked,
            matched,
            candidates,
        }) => {
            assert_eq!(asked, "render");
            assert_eq!(matched, Matched::QualifiedName);
            assert_eq!(candidates.len(), 2, "{candidates:?}");
        }
        other => panic!("expected AmbiguousTarget, got {other:?}"),
    }
}

#[test]
fn a_bare_name_resolves_to_a_declaration_and_arrives_with_its_container() {
    // A method's qualified name is `AuditTrail.append`, so the bare-name lookup is the only one
    // that can find it — and the containment edge is what brings its type along.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("append", generous(&query))
        .expect("compile the method");

    assert_eq!(pack.target.matched, Matched::Name);
    assert_eq!(pack.target.id(), &append_id());
    assert_eq!(
        pack.units.len(),
        2,
        "the method and what contains it: {ids:?}",
        ids = pack.unit_ids()
    );
    assert_eq!(pack.units[0].reason, InclusionReason::Target);
    assert!(
        matches!(pack.units[1].reason, InclusionReason::Container { .. }),
        "a method arrives with its type: {}",
        pack.units[1].reason.describe()
    );
    assert_eq!(pack.units[1].entity.id, audit_trail_id());
}

#[test]
fn an_unknown_target_says_which_of_the_three_lookups_missed() {
    let (store, _dir) = fixture();
    let query = query(&store);

    match query.peek("no_such_symbol", generous(&query)) {
        Err(QueryError::UnknownTarget {
            query: asked,
            detail,
        }) => {
            assert_eq!(asked, "no_such_symbol");
            // Asserted on what the message has to convey rather than on invented wording: the
            // detail must name the reading that was tried and say it came up empty, so a user who
            // typed a symbol name learns that the name is not indexed *and* which of the three
            // readings was most plausible. An earlier version of this test asserted a phrase the
            // message never contained, which is a test that cannot fail for the right reason.
            assert!(
                detail.contains("no_such_symbol"),
                "the message must name what was looked for: {detail}"
            );
            assert!(
                detail.contains("repository path"),
                "the message must say which reading was tried: {detail}"
            );
        }
        other => panic!("expected UnknownTarget, got {other:?}"),
    }

    match query.peek("src/does/not/exist.rs", generous(&query)) {
        Err(QueryError::UnknownTarget { detail, .. }) => {
            assert!(
                detail.contains("repository path"),
                "a path-shaped query must be told that: {detail}"
            );
        }
        other => panic!("expected UnknownTarget, got {other:?}"),
    }
}

#[test]
fn a_file_target_compiles_the_declarations_it_contains() {
    // A file, a symbol and a path are three different questions and give three different packs.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek(LEDGER, generous(&query))
        .expect("compile the ledger file");

    assert_eq!(pack.target.matched, Matched::Path);
    assert_eq!(pack.target.kind(), EntityKind::File);
    let names: Vec<&str> = pack.units.iter().map(|u| u.entity.id.name()).collect();
    for expected in ["settle", "reconcile", "audit"] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    assert_eq!(pack.units[0].reason, InclusionReason::Target);
    for unit in &pack.units[1..] {
        assert!(
            matches!(unit.reason, InclusionReason::Member { .. }),
            "a declaration of a file target is a member: {}",
            unit.reason.describe()
        );
    }
}

#[test]
fn a_symbol_target_and_a_file_target_give_different_answers() {
    // A file's neighbourhood is its content; a symbol's is its context. The two must not be the
    // same pack with a different heading.
    let (store, _dir) = fixture();
    let query = query(&store);
    let budget = generous(&query);

    let by_path = query.peek(LEDGER, budget).expect("compile the file");
    let by_name = query.peek("settle", budget).expect("compile the symbol");

    assert_eq!(by_path.target.matched, Matched::Path);
    assert_eq!(
        by_name.target.matched,
        Matched::QualifiedName,
        "a top-level function's qualified name *is* its bare name, so the qualified-name lookup \
         answers first — the order `resolve_target` documents"
    );
    assert!(includes(&by_path, &settle_id()));
    assert!(
        includes(&by_path, &ledger_file()),
        "a file target's pack is about the file: {ids:?}",
        ids = by_path.unit_ids()
    );
    assert!(
        !includes(&by_path, &process_id()),
        "a file's neighbourhood is its content, not its callers: {ids:?}",
        ids = by_path.unit_ids()
    );
    assert!(
        includes(&by_name, &process_id()),
        "a symbol's neighbourhood includes who calls it: {ids:?}",
        ids = by_name.unit_ids()
    );
    assert!(
        !includes(&by_name, &ledger_file()),
        "the declaration site is already on every unit line, so it costs a unit for nothing: \
         {ids:?}",
        ids = by_name.unit_ids()
    );
    assert_eq!(
        by_path.budget.status,
        BudgetStatus::Complete,
        "{}",
        by_path.report
    );
    assert_eq!(
        by_name.budget.status,
        BudgetStatus::Complete,
        "{}",
        by_name.report
    );
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn two_runs_over_an_unchanged_index_produce_identical_answers() {
    // Without this the tool is untestable: every recorded answer would be a single sample.
    let (store, _dir) = fixture();
    let budget = generous(&query(&store));
    let query = query(&store);

    let first = query.peek("process", budget).expect("compile once");
    let second = query.peek("process", budget).expect("compile again");
    assert_eq!(first, second, "two runs over an unchanged index must agree");
    assert_eq!(first.render(true), second.render(true));

    let walked = query.dependents(&settle_id(), 2).expect("walk");
    let again = query.dependents(&settle_id(), 2).expect("walk");
    assert_eq!(walked, again);

    let explained = query.explain(&process_id()).expect("explain");
    let explained_again = query.explain(&process_id()).expect("explain");
    assert_eq!(explained, explained_again);
}

#[test]
fn a_pack_is_a_value_that_survives_serialisation() {
    // Contract D2: one structured object, rendered identically by every surface. If the JSON and
    // the struct could disagree, "identically" would be a claim rather than a fact.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("process", generous(&query))
        .expect("compile the neighbourhood");

    let json = serde_json::to_string(&pack).expect("serialise the pack");
    let back: ContextPack = serde_json::from_str(&json).expect("read the pack back");
    assert_eq!(pack, back, "a round trip changed the pack");
}

#[test]
fn the_walk_reports_itself_in_one_readable_line() {
    let (store, _dir) = fixture();
    let walk = query(&store)
        .dependents(&settle_id(), 1)
        .expect("walk from settle");
    let headline = walk.headline();
    assert!(headline.contains("is used by"), "{headline}");
    assert!(headline.contains("3 step(s) found"), "{headline}");
    assert!(headline.contains("1 hop(s)"), "{headline}");
}

#[test]
fn a_relations_cost_is_recorded_where_it_can_be_checked() {
    // Every price a pack quotes must be recomputable from the pack itself, or the budget is a
    // claim rather than a measurement.
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("process", generous(&query))
        .expect("compile the neighbourhood");
    let counter = pack.budget.counter;

    for edge in pack.edges() {
        let unit = &pack.units[0].entity.id;
        assert_eq!(
            edge.cost,
            Cost::of(&edge.render(unit, true), counter),
            "an edge's declared cost must be the cost of its own rendering"
        );
    }
    assert!(
        !pack.units[0].entity.doc.as_deref().unwrap_or("").is_empty(),
        "the fixture's declarations carry a doc comment, so the price covers more than a name"
    );
    // A declaration in the fixture has a span, so the rendered line has a position to point at.
    assert!(
        pack.units[0].entity.span.is_some(),
        "a line with no span renders as a bare name, so the reader cannot find it"
    );
}

#[test]
fn the_pack_renders_its_report_and_every_unit_with_its_edges() {
    let (store, _dir) = fixture();
    let query = query(&store);
    let pack = query
        .peek("process", generous(&query))
        .expect("compile the neighbourhood");
    let rendered = pack.render(true);

    assert!(rendered.starts_with("peek context pack"), "{rendered}");
    assert!(
        rendered.contains("counted as: ceil(utf8 bytes / 3)"),
        "the counting rule must be in the answer: {rendered}"
    );
    for unit in &pack.units {
        assert!(
            rendered.contains(&unit.render()),
            "every unit must appear in the rendering: {}",
            unit.entity.id
        );
    }
    for edge in pack.edges() {
        assert!(
            rendered.contains(&edge.render(&pack.units[0].entity.id, true)),
            "every edge must appear in the rendering"
        );
    }
}

#[test]
fn an_ambiguous_edge_whose_candidates_the_relation_lost_is_repaired_from_the_store() {
    // A relation that read back with an empty candidate list is a real, reportable state: the
    // resolver found more than one match and failed to record them. `explain` re-reads the rows
    // rather than printing "ambiguous (0 candidates)", and says that it did.
    let (store, _dir) = fixture();
    let query = query(&store);
    let edge = store
        .outgoing(&process_id(), Some(RelationKind::Calls), 50)
        .expect("read the calls of process")
        .into_iter()
        .find(|relation| relation.resolution.is_ambiguous())
        .expect("process has an ambiguous call");
    let stripped = Relation {
        resolution: ResolutionState::Ambiguous {
            candidates: Vec::new(),
        },
        ..edge
    };

    let explained = query.explain_relation(&stripped).expect("explain it");
    assert_eq!(explained.edges[0].candidate_count(), 2);
    assert_eq!(explained.edges[0].state, "ambiguous (2 candidates)");
    assert!(
        explained
            .notes
            .iter()
            .any(|note| note.contains("re-read from `relation_candidate`")),
        "the repair must be disclosed: {notes:?}",
        notes = explained.notes
    );
}

#[test]
fn an_ambiguous_edge_with_no_candidate_rows_at_all_is_reported_as_such() {
    // The other half: if the rows are missing too, there is nothing to repair with, and saying
    // "ambiguous (0 candidates)" without comment would read as "there were no candidates".
    let (store, _dir) = fixture();
    let query = query(&store);
    let edge = store
        .outgoing(&process_id(), Some(RelationKind::Calls), 50)
        .expect("read the calls of process")
        .into_iter()
        .find(|relation| relation.resolution.is_ambiguous())
        .expect("process has an ambiguous call");
    // A name the candidate table never recorded.
    let stripped = Relation {
        target_name: "render_nowhere".to_owned(),
        resolution: ResolutionState::Ambiguous {
            candidates: Vec::new(),
        },
        ..edge
    };

    let explained = query.explain_relation(&stripped).expect("explain it");
    assert_eq!(explained.edges[0].state, "ambiguous (no candidates)");
    assert!(
        explained
            .notes
            .iter()
            .any(|note| note.contains("did not record them")),
        "{notes:?}",
        notes = explained.notes
    );
}

#[test]
fn the_walk_headline_and_options_survive_serialisation() {
    // An answer that says what produced it has to be able to travel to a CLI or an MCP client
    // without the reader losing the policy.
    let (store, _dir) = fixture();
    let walk = query(&store)
        .dependents(&settle_id(), 1)
        .expect("walk from settle");
    let json = serde_json::to_string(&walk).expect("serialise the walk");
    let back: Walk = serde_json::from_str(&json).expect("read the walk back");
    assert_eq!(walk, back);
    assert!(json.contains("\"follow_inferred\""), "{json}");
}
