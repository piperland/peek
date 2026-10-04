//! What a `References` edge is worth, measured on a real index.
//!
//! The engine declared `RelationKind::References` and could not produce one: the gate measured
//! `references` at 0 of 18 and the reason was a rule, not a grammar. That makes the class's
//! *existence* cheap to argue about and its *value* the real question. An edge kind that resolves
//! nothing and answers nothing is not worth storing, and the honest answer here might have been to
//! delete it.
//!
//! So this file asks the two questions that decide it, through the public surface, on an index
//! built by the real indexer from real source:
//!
//! * **Is the edge followable?** A relation nobody can traverse is a record, not an answer.
//! * **Does it change an answer?** The same query, over the same index, at the same depth and
//!   about the same target, asked with and without reference edges. If the two answers are the
//!   same, the edge is redundant with something already there and the case for it is gone.
//!
//! The second is the measurement the first cannot substitute for. `WalkRequest` takes a relation
//! kind as a filter, so "what would this query find if references did not count" is one argument
//! away rather than a hypothetical.
//!
//! The fixture is chosen so the two answers **differ**. `encode` calls `helper` and also *reads*
//! two constants. The call is what a `Calls` edge records. The two reads are what nothing else in
//! the engine records: Peek emits no `reads` edge, and no `uses_type` edge is emitted for either,
//! so a reference is the only row that says `encode` depends on `LIMIT`.

#![allow(clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use peek_core::discover::DiscoveryOptions;
use peek_core::indexer;
use peek_core::model::{Entity, EntityKind, RelationKind};
use peek_core::query::{Query, WalkRequest};
use peek_core::store::Store;

/// A crate whose whole point is one call and two name uses.
const LIB: &str = "\
//! A crate with one call and two name uses.

/// The largest value a frame may hold.
pub const LIMIT: u8 = 64;

/// How far a codec advances in one step.
pub const STEP: u8 = 1;

/// Does nothing but add one.
pub fn helper(value: u8) -> u8 {
    value + 1
}

/// Wraps a value, bounded by the frame limit.
pub fn encode(value: u8) -> u8 {
    let mut total = LIMIT;
    total += STEP;
    helper(total)
}
";

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A tree that deletes itself, so a failing test leaves nothing behind.
struct Tree(PathBuf);

impl Tree {
    fn new() -> Self {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "peek-reference-value-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("src")).expect("create the tree");
        fs::write(path.join("Cargo.toml"), "[package]\nname = \"probe\"\n")
            .expect("write the manifest");
        fs::write(path.join("src/lib.rs"), LIB).expect("write the source");
        Self(path)
    }

    fn root(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An index of [`Tree`], built by the real indexer.
fn indexed(tree: &Tree) -> Store {
    // The programmatic override rather than the environment variable, because the environment is
    // process-global and the test binary runs its tests concurrently. A test that wrote into a
    // developer's real cache would be a test nobody trusts.
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "peek-reference-value-index-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create the index root");
    peek_core::store::paths::set_root_override(Some(root));

    let mut store = indexer::open_store(tree.root()).expect("open the store");
    indexer::build_full(&mut store, tree.root(), DiscoveryOptions::default())
        .expect("a full build of the probe crate");
    store.verify().expect("the index verifies");
    store
}

/// The one entity in the fixture with this qualified name.
fn entity(store: &Store, qualified_name: &str) -> Entity {
    let found = store
        .entities_with_qualified_name(qualified_name, 16)
        .expect("look the entity up by qualified name");
    assert_eq!(
        found.len(),
        1,
        "`{qualified_name}` is declared once in this fixture, and {found:?} were found"
    );
    found.into_iter().next().expect("the one entity")
}

/// The kinds of the relations leaving `source` that name `target_name`, in the store's order.
fn kinds_named(store: &Store, source: &Entity, target_name: &str) -> Vec<RelationKind> {
    let mut kinds: Vec<RelationKind> = store
        .outgoing(&source.id, None, 512)
        .expect("read the outgoing edges")
        .iter()
        .filter(|relation| relation.target_name == target_name)
        .map(|relation| relation.kind)
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

#[test]
fn a_use_of_a_name_is_decided_rather_than_left_pending() {
    // The first question, and the one an edge has to pass before it is worth anything: does the
    // engine actually decide where it points? An unresolved relation is a record of a name, and a
    // consumer cannot act on it. `references` reaching a non-zero score in the matrix is not
    // enough on its own — a class that produced only unresolved rows would score identically.
    let tree = Tree::new();
    let store = indexed(&tree);
    let encode = entity(&store, "encode");

    let edges = store
        .outgoing(&encode.id, Some(RelationKind::References), 512)
        .expect("read the reference edges");
    assert!(
        !edges.is_empty(),
        "`encode` reads two constants and passes a local to a call; the engine emitted no \
         reference edges from it at all"
    );

    let limit = edges
        .iter()
        .find(|relation| relation.target_name == "LIMIT")
        .expect("the reference to `LIMIT`");
    assert!(
        limit.is_followable(),
        "`LIMIT` is declared in this same file, so the edge has to be decided for a traversal to \
        reach anything: {}",
        limit.resolution.describe()
    );
    assert_eq!(
        limit.target.as_ref().map(|target| target.kind()),
        Some(EntityKind::Constant),
        "`LIMIT` is a constant, and the edge names the constant rather than something else"
    );

    // And the class does not have to pretend. `total` is a local binding, so nothing in the
    // repository declares it; the edge is reported with that reason rather than resolved to the
    // nearest thing that happens to share the name, which is what makes the count of *decided*
    // reference edges mean something.
    let local = edges
        .iter()
        .find(|relation| relation.target_name == "total")
        .expect("the reference to the local `total`");
    assert!(
        !local.is_followable(),
        "nothing declares `total`, so the edge must not claim a target: {}",
        local.resolution.describe()
    );
    assert_eq!(
        local.target_name, "total",
        "and it keeps the name as written, which is what makes the row readable"
    );
}

#[test]
fn a_reference_edge_changes_an_answer_that_nothing_else_answers() {
    // The measurement the whole class rests on. Two walks over one index, same target, same
    // depth, same direction: one counts every dependency kind and one counts only calls. If the
    // answers were the same the edge would be redundant with a `Calls` edge and there would be
    // nothing to say for storing it.
    let tree = Tree::new();
    let store = indexed(&tree);
    let query = Query::new(&store);
    let encode = entity(&store, "encode");

    // The control: `helper` is reached by a call, so both walks agree. Without it, the
    // comparison below would be satisfied by a pair of walks that both find nothing.
    let helper = entity(&store, "helper");
    let every_kind = query
        .dependents(&helper.id, 1)
        .expect("walk to everything that depends on `helper`");
    let calls_only = query
        .walk(
            &helper.id,
            WalkRequest::dependents(1).with_kind(Some(RelationKind::Calls)),
        )
        .expect("walk to only what calls `helper`");
    assert!(
        every_kind.contains(&encode.id),
        "`encode` calls `helper`, so a walk counting every dependency kind must reach it; it \
         reached {} of {} steps",
        every_kind.steps.len(),
        every_kind.inspected
    );
    assert!(
        calls_only.contains(&encode.id),
        "and so must a walk counting only calls; it reached {} of {} steps",
        calls_only.steps.len(),
        calls_only.inspected
    );

    // The case that decides it. `LIMIT` and `STEP` are read, not called, and the engine emits no
    // `reads` edge for either, so the reference is the only row recording the dependency.
    for name in ["LIMIT", "STEP"] {
        let target = entity(&store, name);
        assert_eq!(
            target.kind,
            EntityKind::Constant,
            "`{name}` is a constant in this fixture"
        );

        let via_every_kind = query
            .dependents(&target.id, 1)
            .expect("walk to everything that depends on the constant");
        assert!(
            via_every_kind.contains(&encode.id),
            "`encode` reads `{name}`, so a walk counting every dependency kind must reach it; it \
             reached {} of {} steps",
            via_every_kind.steps.len(),
            via_every_kind.inspected
        );

        let via_calls_only = query
            .walk(
                &target.id,
                WalkRequest::dependents(1).with_kind(Some(RelationKind::Calls)),
            )
            .expect("walk to only what calls the constant");
        assert!(
            !via_calls_only.contains(&encode.id),
            "nothing calls `{name}`, so a walk restricted to call edges must not reach `encode`; \
             it reached {} steps",
            via_calls_only.steps.len()
        );

        // And the step says how it arrived, so the difference is attributable rather than merely
        // observed.
        let arrived = via_every_kind
            .steps
            .iter()
            .find(|step| step.id == encode.id)
            .expect("`encode` is among the steps");
        assert_eq!(
            arrived.via.kind,
            RelationKind::References,
            "the only route to `{name}` is the reference, and the step has to say so"
        );
    }
}

#[test]
fn the_reference_edges_the_fixture_produces_are_the_ones_the_source_writes() {
    // Shape, stated exactly, because "did the class become reachable" is answered by a count and a
    // count nobody checked is a number nobody took. Read the source: `encode` invokes `helper`,
    // reads `LIMIT`, reads `STEP` and names a local twice. `helper` invokes nothing and names only
    // its own parameter, which is a use of a name and is expected.
    let tree = Tree::new();
    let store = indexed(&tree);
    let encode = entity(&store, "encode");
    let helper = entity(&store, "helper");

    assert_eq!(
        kinds_named(&store, &encode, "helper"),
        vec![RelationKind::Calls, RelationKind::References],
        "`helper(total)` is an invocation *and* a use of the name `helper`: the call says what was \
         invoked and the reference says what was named, and the two are separate rows"
    );
    assert_eq!(
        kinds_named(&store, &encode, "LIMIT"),
        vec![RelationKind::References],
        "reading a constant is a use of a name and nothing else, which is the argument for the \
         class"
    );
    assert_eq!(
        kinds_named(&store, &encode, "STEP"),
        vec![RelationKind::References],
        "reading a constant is a use of a name and nothing else"
    );
    assert_eq!(
        kinds_named(&store, &encode, "encode"),
        Vec::<RelationKind>::new(),
        "the function declares `encode` and never names it inside itself"
    );
    assert_eq!(
        kinds_named(&store, &helper, "helper"),
        Vec::<RelationKind>::new(),
        "and so does `helper`, whose body names only its parameter"
    );
}
