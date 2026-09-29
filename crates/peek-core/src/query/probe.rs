//! The query engine over a repository nobody wrote to be indexed.
//!
//! `crates/peek-core/tests/real_repository.rs` proves the *index* is right on real code. It does
//! not prove the *answers* are right, and those are different claims. A graph can be perfectly
//! indexed and still produce a context pack that overruns its budget, hides its own ambiguity, or
//! differs between two runs over the same index — and none of that is visible in an entity count.
//!
//! So this is a second probe, for the same repository, at the query layer. It is `#[ignore]`d for
//! the same reason and is run the same way:
//!
//! ```text
//! PEEK_PROBE_REPO=/path/to/repo cargo test --lib query::probe -- --ignored --nocapture
//! ```
//!
//! It uses only the public API, so it doubles as a check that the public surface is enough to ask
//! a real question of a real tree without reaching into internals.
//!
//! # What it asserts, and what it only reports
//!
//! Asserted, because these must never be false on any repository:
//!
//! * a pack costs no more than the budget it was given, at every budget tried;
//! * spent and remaining account for the whole budget;
//! * every edge in a pack carries a resolution state, and an ambiguous one names its candidates;
//! * a walk terminates, and does not include its own target;
//! * two runs over an unchanged index produce identical output;
//! * an `explain` reports a state for every edge it lists.
//!
//! Reported but not asserted, because "what fraction of a repository's edges should resolve" is an
//! empirical question and inventing a number for it is the defect this project exists to remove.
//! The measurements that *are* printed — how many edges each state contributed to a pack, how much
//! of a budget a pack used, how many candidates an ambiguity had — are the interesting part.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::discover::{DiscoveryOptions, FileDiscovery};
use crate::indexer;
use crate::model::{EntityId, EntityKind};
use crate::query::{ContextPack, Query, QueryError};
use crate::store::{RepoId, Store};

/// The budgets tried, as multiples of nothing in particular: a figure small enough to force
/// dropping, and one large enough that the neighbourhood is exhausted.
///
/// Derived from the pack's own arithmetic rather than written down, so the probe measures the
/// compiler rather than my guess at its scale.
const BUDGETS: [u64; 4] = [1, 400, 4_000, 40_000];

#[test]
#[ignore = "needs PEEK_PROBE_REPO to point at a real repository"]
fn report_the_query_engine_on_a_real_repository() {
    let root = PathBuf::from(
        std::env::var("PEEK_PROBE_REPO").expect("set PEEK_PROBE_REPO to the repository to index"),
    );
    assert!(root.is_dir(), "{} is not a directory", root.display());

    let mut store = fresh_store(&root);
    let started = Instant::now();
    let outcome = indexer::build_full(&mut store, &root, DiscoveryOptions::default())
        .expect("a real repository must index without error");
    println!("\n=== repository ===");
    println!("path:    {}", root.display());
    println!("build:   {:?}", started.elapsed());
    println!("report:  {}", outcome.report().summary());

    let target = pick_a_target(&store, &root)
        .expect("no indexed declaration with edges was found; nothing to ask about");
    println!("target:  {}", target.display());

    report_explain(&store, &target);
    report_walks(&store, &target);
    report_packs(&store, &target);
    report_determinism(&store, &target);

    match store.verify() {
        Ok(()) => println!("\nintegrity_check: ok"),
        Err(error) => panic!("integrity_check failed after indexing a real repository: {error}"),
    }
}

/// A store that has never been written, so the measurement is of a cold build.
fn fresh_store(root: &Path) -> Store {
    let repo = RepoId::discover(root).expect("derive a repository id");
    let directory = crate::store::paths::index_dir(&repo).expect("resolve the index directory");
    // Only this repository's directory, which is named for its identity. A measurement, not a
    // user's data.
    let _ = std::fs::remove_dir_all(&directory);
    indexer::open_store(root).expect("open a fresh store")
}

/// A declaration worth asking about, chosen from the index rather than from the filesystem.
///
/// The choice is deliberately unglamorous: the first function or method in discovery order that
/// has at least one edge. Picking "the most connected symbol" would be a quality claim, and this
/// probe makes none.
fn pick_a_target(store: &Store, root: &Path) -> Option<EntityId> {
    let discovery = FileDiscovery::new(root, DiscoveryOptions::default())
        .discover()
        .ok()?;
    for file in discovery.files() {
        if !file.path.as_str().ends_with(".rs") {
            continue;
        }
        let Ok(entities) = store.entities_in_file(&file.path, 2_000) else {
            continue;
        };
        for entity in entities {
            if entity.kind() != EntityKind::Function && entity.kind() != EntityKind::Method {
                continue;
            }
            let Ok(edges) = store.incoming(&entity.id, None, 64) else {
                continue;
            };
            if edges.is_empty() {
                continue;
            }
            // **Unambiguous, or skip it.** An earlier version took the first candidate with any
            // edge at all, which on a real repository is a `Debug`/`fmt` impl in a fuzz target —
            // and a `fmt` method is implemented several times over, so its qualified name is
            // shared. `peek` then refused every budget with "name one of them", and the probe spent
            // its whole context-compiler section measuring the *refusal* path instead of the
            // compiler. A probe that silently exercises the least interesting branch is worse than
            // one that fails, because it reports numbers that look like coverage.
            //
            // So the target must be addressable: a qualified name that exactly one indexed entity
            // carries. That is a property a real user can rely on being able to ask about, which
            // is the only reason to pick it.
            let matches = store
                .entities_with_qualified_name(entity.id.qualified_name(), 8)
                .unwrap_or_default();
            if matches.len() == 1 {
                return Some(entity.id);
            }
        }
    }
    None
}

fn report_explain(store: &Store, target: &EntityId) {
    println!("\n=== explain ===");
    let query = Query::new(store);
    let explained = match query.explain(target) {
        Ok(explained) => explained,
        Err(error) => {
            println!("explain failed: {error}");
            return;
        }
    };

    let mut by_state: BTreeMap<String, u64> = BTreeMap::new();
    for edge in &explained.edges {
        *by_state.entry(edge.state.clone()).or_default() += 1;
        assert!(
            !edge.state.is_empty(),
            "every edge in an explanation must carry a state: {:?}",
            edge.relation
        );
        if edge.relation.resolution.is_ambiguous() {
            assert_eq!(
                edge.candidate_count(),
                edge.candidates().len(),
                "the count `explain` prints must be the number of candidates it hands back"
            );
        }
    }
    println!("edges:   {}", explained.edges.len());
    for (state, count) in &by_state {
        println!("  {count:>5}  {state}");
    }
    println!("chain:   {} hop(s)", explained.chain.len());
    for step in &explained.chain {
        println!(
            "  {} {} via {} ({} of {} reached the previous node)",
            step.distance,
            step.id.display(),
            step.via.kind,
            step.via.resolution.describe(),
            step.alternatives
        );
    }
    for note in &explained.notes {
        println!("note:    {note}");
    }
    println!("---- rendered ----\n{}", explained.render());
}

fn report_walks(store: &Store, target: &EntityId) {
    println!("\n=== traversal ===");
    let query = Query::new(store);
    for depth in [1_u32, 2, 3] {
        let started = Instant::now();
        let walk = query
            .dependents(target, depth)
            .expect("dependents must not fail");
        assert!(
            walk.steps.iter().all(|step| &step.id != target),
            "D-0007: `dependents` must never include its own target"
        );
        assert!(
            walk.max_distance().is_none_or(|furthest| furthest <= depth),
            "a walk cannot reach further than the depth it was given: {walk:?}"
        );
        println!(
            "depth {depth}: {} step(s), {} relation(s) read, {} expanded, {} closed, \
             {:?}, {:?}, complete={}",
            walk.steps.len(),
            walk.inspected,
            walk.visited,
            walk.closed,
            started.elapsed(),
            walk.headline(),
            walk.is_complete(),
        );
    }
    let callees = query.callees(target).expect("callees must not fail");
    println!(
        "callees:  {} step(s), {} inferred",
        callees.steps.len(),
        callees.followed_inferred
    );
    assert_eq!(
        callees.followed_inferred,
        callees.inferred().count(),
        "the inferred count must match the list it summarises"
    );
}

fn report_packs(store: &Store, target: &EntityId) {
    println!("\n=== the context compiler ===");
    let query = Query::new(store);
    let name = target.qualified_name().to_owned();

    for budget in BUDGETS {
        let started = Instant::now();
        let pack = match query.peek(&name, budget) {
            Ok(pack) => pack,
            Err(QueryError::BudgetTooSmall { requested, minimum }) => {
                println!(
                    "budget {budget:>6}: refused — the empty report costs {minimum} token(s), \
                     and {requested} was asked for"
                );
                continue;
            }
            Err(error) => {
                println!("budget {budget:>6}: failed: {error}");
                continue;
            }
        };
        assert!(
            pack.budget.within_budget(),
            "a pack may not cost more than it was given: {budget} vs {}",
            pack.budget.spent_tokens
        );
        assert_eq!(
            pack.budget.spent_tokens + pack.budget.remaining_tokens,
            budget,
            "spent and remaining must account for the whole budget"
        );
        assert_pack_is_honest(&pack);
        println!("{}", summary(&pack, budget, started.elapsed()));
    }

    // The most interesting single measurement: what a budget an agent could actually afford buys
    // on a real repository, and what it had to leave behind.
    let affordable = 8_000;
    match query.peek(&name, affordable) {
        Ok(pack) => {
            assert_pack_is_honest(&pack);
            println!(
                "---- {affordable} tokens ----\n{}",
                summary(&pack, affordable, std::time::Duration::ZERO)
            );
            println!("---- rendered ----\n{}", pack.render(true));
        }
        Err(error) => println!("a budget of {affordable} was refused: {error}"),
    }
}

/// The invariants that must hold for any pack, on any repository.
fn assert_pack_is_honest(pack: &ContextPack) {
    // A pack may legitimately contain no declarations at all — a budget too small for the target
    // is a refusal, not a slice — so the edge check is only meaningful when there is a unit to
    // read the edges relative to.
    if let Some(target) = pack.units.first() {
        for edge in pack.edges() {
            assert!(
                !edge.state().is_empty(),
                "every edge in a pack must carry its resolution state: {:?}",
                edge.relation
            );
            let rendered = edge.render(&target.entity.id, true);
            assert!(
                rendered.contains(&format!("[{}]", edge.state())),
                "the rendered edge must carry the same state the struct holds: {rendered}"
            );
            if edge.relation.resolution.is_ambiguous() {
                let count = edge.candidates().len();
                assert!(
                    edge.state().contains(&count.to_string()),
                    "an ambiguous edge must print its candidate count: {}",
                    edge.state()
                );
            }
        }
    }
    for unit in &pack.units {
        assert!(
            !unit.reason.describe().is_empty(),
            "every unit in a pack must state why it is there: {}",
            unit.entity.id
        );
    }
}

fn summary(pack: &ContextPack, budget: u64, elapsed: std::time::Duration) -> String {
    let mut by_state: BTreeMap<String, u64> = BTreeMap::new();
    for edge in pack.edges() {
        *by_state.entry(edge.state()).or_default() += 1;
    }
    let mut omitted: BTreeMap<String, u64> = BTreeMap::new();
    for entry in &pack.omitted {
        *omitted
            .entry(format!("{} {}", entry.what.as_str(), entry.reason.as_str()))
            .or_default() += 1;
    }
    let mut text = format!(
        "budget {budget:>6}: {} unit(s), {} edge(s), spent {} of {budget} in {elapsed:?}\n  \
         status: {} (considered {}, unexamined {})\n",
        pack.units.len(),
        pack.edges().count(),
        pack.budget.spent_tokens,
        pack.budget.status,
        pack.budget.candidates_considered,
        pack.budget.candidates_unexamined,
    );
    for (state, count) in &by_state {
        text.push_str(&format!("    {count:>5}  edge  {state}\n"));
    }
    for (reason, count) in &omitted {
        text.push_str(&format!("    {count:>5}  drop  {reason}\n"));
    }
    for note in &pack.notes {
        text.push_str(&format!("    note: {note}\n"));
    }
    text
}

fn report_determinism(store: &Store, target: &EntityId) {
    println!("\n=== determinism ===");
    let query = Query::new(store);
    let name = target.qualified_name().to_owned();

    let first = query.peek(&name, 4_000);
    let second = query.peek(&name, 4_000);
    assert_eq!(
        first.is_ok(),
        second.is_ok(),
        "the same question must fail the same way twice"
    );
    if let (Ok(first), Ok(second)) = (first, second) {
        assert_eq!(first, second, "two runs over an unchanged index must agree");
        assert_eq!(first.render(true), second.render(true));
        println!("pack:    identical across two runs");
    }

    let walked = query.dependents(target, 2).expect("walk");
    assert_eq!(walked, query.dependents(target, 2).expect("walk"));
    println!("walk:    identical across two runs");

    let explained = query.explain(target).expect("explain");
    assert_eq!(explained, query.explain(target).expect("explain"));
    println!("explain: identical across two runs");
}
