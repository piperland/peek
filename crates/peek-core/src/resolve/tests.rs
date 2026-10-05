//! The resolver's conformance suite.
//!
//! These tests are about **what the index claims**, not about how the ladder reaches the answer.
//! Every one of them builds a real repository on disk, runs a real extraction and a real
//! resolution, and reads the decisions back through the store's public query surface — the same
//! path `peek explain` and the MCP server will use. A test that asserted on the resolver's
//! internal rung would keep passing while the engine started answering questions wrongly.
//!
//! The defects they guard are the ones the engine Peek replaces actually had. Each test's comment
//! names the finding, so a future reader can check that the failure it prevents is the one that
//! was described.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{ResolutionOptions, resolve_all, resolve_paths, rule_name, rung_name};
use crate::discover::DiscoveryOptions;
use crate::indexer::{IndexOutcome, build_full};
use crate::model::entity::{EntityId, EntityKind};
use crate::model::path::RepoPath;
use crate::model::relation::{Evidence, Relation, RelationKind, ResolutionState, UnresolvedReason};
use crate::store::{RepoId, Store, StoreError};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A temporary repository, with its index deliberately outside the tree.
///
/// D-0006 puts the real cache under the OS cache root, and a test that puts the database inside
/// the repository is not testing the deployed layout: discovery would walk the `.db` and its WAL
/// sidecars and report three unsupported files that exist only because the test put them there.
struct TempTree {
    root: PathBuf,
    db: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = format!("peek-resolve-{}-{label}-{unique}", std::process::id());
        let root = std::env::temp_dir().join(format!("{base}-tree"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create the temporary tree");
        let db = std::env::temp_dir().join(format!("{base}-index/index.db"));
        if let Some(parent) = db.parent() {
            fs::create_dir_all(parent).expect("create the index directory");
        }
        Self { root, db }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the parent directory");
        }
        fs::write(&path, contents).expect("write the fixture file");
    }

    /// Extract and persist, without resolving. The starting state for every test here.
    fn index_without_resolving(&self) -> Store {
        let repo = RepoId::discover(self.path()).expect("derive a repository id");
        let mut store = Store::open(&self.db, &repo).expect("open the store");
        // The indexer always resolves, so the pending state is produced by indexing with
        // resolution suppressed. Doing it by hand keeps the tests independent of the order the
        // two stages run in.
        let discovery =
            crate::discover::FileDiscovery::new(self.path(), DiscoveryOptions::default())
                .discover()
                .expect("discover");
        let mut update = crate::store::IndexUpdate::empty();
        for file in discovery.files() {
            let absolute = discovery.report().absolute(&file.path);
            let Some(spec) = crate::extract::registry::get(file.language) else {
                continue;
            };
            let text = match fs::read_to_string(&absolute) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let extracted = crate::extract::extract_with(spec, file.path.clone(), &text);
            update = update.removing_file(extracted.path.clone());
            for entity in extracted.entities {
                update = update.with_entity(entity);
            }
            for relation in extracted.relations {
                update = update.with_relation(relation);
            }
        }
        store.apply_update(update).expect("commit the extraction");
        store
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        if let Some(parent) = self.db.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}

fn id(path: &str, kind: EntityKind, qualified: &str) -> EntityId {
    EntityId::new(RepoPath::new(path).expect("valid path"), kind, qualified, 0)
}

/// Every relation of a kind, whatever state it is in.
fn relations_of(store: &Store, kind: RelationKind) -> Vec<crate::model::Relation> {
    let mut found = Vec::new();
    for state in [
        ResolutionState::Pending {
            evidence: Evidence::NameOnly,
            basis: String::new(),
        },
        ResolutionState::Resolved {
            by: Evidence::NameOnly,
        },
        ResolutionState::Inferred {
            by: Evidence::NameOnly,
            basis: String::new(),
        },
        ResolutionState::Ambiguous {
            candidates: Vec::new(),
        },
        ResolutionState::Unresolved {
            reason: UnresolvedReason::NoCandidate,
        },
    ] {
        if let Ok(rows) = store.relations_in_state(&state, 512) {
            found.extend(rows.into_iter().filter(|row| row.kind == kind));
        }
    }
    found
}

/// The one call edge in the index, or a message saying what was found instead.
fn the_call(store: &Store, target_name: &str) -> crate::model::Relation {
    let mut calls: Vec<crate::model::Relation> = relations_of(store, RelationKind::Calls)
        .into_iter()
        .filter(|relation| relation.target_name == target_name)
        .collect();
    match calls.len() {
        1 => calls.remove(0),
        other => panic!(
            "expected exactly one call to `{target_name}`, found {other}; the index holds {}",
            relations_of(store, RelationKind::Calls).len()
        ),
    }
}

/// The state of a relation, as a string, for a failure message.
fn state_of(relation: &crate::model::Relation) -> String {
    format!(
        "{} -> {} ({}), target {:?}",
        relation.source.qualified_name(),
        relation.target_name,
        relation.resolution.describe(),
        relation.target.as_ref().map(ToString::to_string),
    )
}

/// The evidence a `Resolved` relation was resolved on, or a message saying it was not resolved.
///
/// A rung that fired is not observable from outside, so a test that asserts *which* rung answered
/// reads it out of the stored state — the same field `peek explain` reports — rather than out of
/// the resolver. That is the only assertion that would fail if `explain` and the ladder disagreed.
fn decided_by(relation: &crate::model::Relation) -> Evidence {
    match &relation.resolution {
        ResolutionState::Resolved { by } => by.clone(),
        other => panic!(
            "expected a resolved relation, got {other:?}: {}",
            state_of(relation)
        ),
    }
}

/// Reset one kind of relation to `Pending`, resolve the index again with the module table on or
/// off, and hand back the relations of that kind.
///
/// Both arms come out of **one** extraction, which is the only kind of comparison worth making:
/// two builds is what produced R-010's convincing false trend, and an arm that reads the other
/// arm's rows is not a comparison at all. So the relations are rebuilt from the rows already in
/// the index and the pass runs again over them. The evidence is carried forward so both arms
/// choose a rung the same way, and the target is dropped rather than carried, because a pending
/// row that still names a target is a decided edge wearing a pending hat. The second handle is
/// closed before this returns, so the next arm opens the index on its own.
fn decide_under(tree: &TempTree, store: &Store, kind: RelationKind, table: bool) -> Vec<Relation> {
    let mut scratch = Store::open(&tree.db, store.repo()).expect("a second handle");
    let mut update = crate::store::IndexUpdate::empty();
    for relation in relations_of(store, kind) {
        let evidence = match &relation.resolution {
            ResolutionState::Pending { evidence, .. } => evidence.clone(),
            ResolutionState::Resolved { by } => by.clone(),
            ResolutionState::Inferred { by, .. } => by.clone(),
            // Neither of these carries evidence, and `NameOnly` is where the extractor itself
            // starts, so a re-decision begins where a first one would.
            ResolutionState::Ambiguous { .. } | ResolutionState::Unresolved { .. } => {
                Evidence::NameOnly
            }
        };
        let pending = Relation::pending(
            kind,
            relation.source.clone(),
            relation.target_name.clone(),
            relation.span,
            evidence,
            "reset so both arms decide the same relations",
        );
        update = update.with_relation(pending);
    }
    scratch
        .apply_update(update)
        .expect("reset the relations to pending");
    let options = ResolutionOptions {
        use_module_table: table,
        ..ResolutionOptions::default()
    };
    resolve_all(&mut scratch, options).expect("resolve");
    relations_of(&scratch, kind)
}

/// The one relation in a list naming `target_name`, or a message saying what was there.
fn the_one<'a>(relations: &'a [Relation], target_name: &str) -> &'a Relation {
    let mut named: Vec<&Relation> = relations
        .iter()
        .filter(|relation| relation.target_name == target_name)
        .collect();
    match named.len() {
        1 => named.remove(0),
        other => panic!(
            "expected exactly one relation naming `{target_name}`, found {other} in {}",
            relations.len()
        ),
    }
}

// ---------------------------------------------------------------------------
// A local binding: the class that names a rule rather than a rung
// ---------------------------------------------------------------------------
//
// Four tests, and they are four rather than one because the rule has four separate ways to be
// wrong. That it fires at all; that it fires *instead of* the ladder rather than after it; that
// the three states carrying the class are all read; and that a relation refused once is refused
// again, which is the half the wire format costs and the half that has to be got right anyway.

/// A repository whose `out` is a `let` in one file and a parameter in another.
///
/// The second declaration is the whole fixture. A rule that only refused when nothing shares the
/// name would pass every test below, so `render.out` is here to be the wrong answer: exactly one
/// entity carries `out` in the repository, which is the condition under which R5 would place the
/// reference and the condition under which its answer would be wrong.
fn local_binding_tree(label: &str) -> TempTree {
    let tree = TempTree::new(label);
    tree.write(
        "src/report.rs",
        "pub fn render(out: &mut String) -> usize {\n    out.len()\n}\n",
    );
    tree.write(
        "src/lib.rs",
        "mod report;\n\npub fn summarise(tag: &str) -> String {\n    let mut out = String::new();\n    \
         out.push_str(tag);\n    report::render(&mut out);\n    out\n}\n",
    );
    tree
}

/// Every relation in one state, wherever the store holds it.
fn relations_in(store: &Store, state: &ResolutionState) -> Vec<Relation> {
    store
        .relations_in_state(state, 512)
        .unwrap_or_else(|error| panic!("read the {state:?} relations: {error}"))
}

/// The references `summarise` writes to `name`, whichever state they ended up in.
///
/// Filtered on the source and the kind as well as the name, because a fixture that names `out`
/// in two declarations produces more relations than the one under test: `render`'s **parameter**
/// declaration is a `Contains` edge from `render` to `render.out`, carrying the same name and
/// nothing to do with the `let` in `summarise`. Filtering by name alone would pick that up, and a
/// test asserting "no decided relation names `out`" would then fail on the containment edge the
/// extractor settled at extraction — a correct edge, decided by the strongest rule in the model.
fn references_from<'a>(relations: &'a [Relation], subject: &str, name: &str) -> Vec<&'a Relation> {
    let found: Vec<&Relation> = relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::References)
        .filter(|relation| relation.source.qualified_name() == subject)
        .filter(|relation| relation.target_name == name)
        .collect();
    assert!(
        !found.is_empty(),
        "no reference to `{name}` was extracted from `{subject}`, so every count below would be \
         over an empty population: {} relations read",
        relations.len()
    );
    found
}

#[test]
fn a_name_a_binder_claims_is_refused_before_the_ladder_runs() {
    // The rule, on a relation that reaches it `Pending`.
    //
    // `out` in `summarise` is bound by a `let`, the extractor records `local_binding`, and the
    // index holds one entity carrying that name — `report.rs`'s `render.out` parameter, which is
    // not what the occurrence means. R5 would place it there and be wrong; the head rule places
    // it nowhere and says why.
    let tree = local_binding_tree("refused-at-the-head");
    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let refused_state = ResolutionState::Unresolved {
        reason: UnresolvedReason::LocalBinding,
    };
    let refusals = relations_in(&store, &refused_state);
    let refused = &references_from(&refusals, "summarise", "out")[0];
    assert_eq!(
        refused.target,
        None,
        "a refused relation points at nothing: {}",
        state_of(refused)
    );
    assert_eq!(
        refused.resolution,
        refused_state,
        "and it says so in the stored state, not only in this test: {}",
        state_of(refused)
    );

    // The counterweight, which is what makes the assertion above mean something: R5's answer for
    // this exact name exists and is reachable, so a rule that fired after the ladder would look the
    // same on this row and differ on none. Read through the *reference* to `render.out` itself,
    // which `summarise` names as an argument — a relation the fixture needs and which must survive,
    // so this is also the check that the refusal did not spread past the occurrences it is about.
    let placed = relations_in(
        &store,
        &ResolutionState::Resolved {
            by: Evidence::NameOnly,
        },
    )
    .into_iter()
    .chain(relations_in(
        &store,
        &ResolutionState::Inferred {
            by: Evidence::NameOnly,
            basis: String::new(),
        },
    ))
    .any(|relation| relation.target_name == "out" && relation.target.is_some());
    assert!(
        placed,
        "no relation to `out` is placed on an entity, so this test would pass over an index in \
         which the name was unplaceable and the rule had nothing to refuse"
    );
    assert_eq!(
        report.unresolved_by_reason.get("local_binding"),
        Some(&4),
        "and the refusal is counted under its own reason rather than folded into no_candidate, \
         because the two are different findings — the one occurrence that writes the binding plus \
         the three uses inside its scope: {}",
        report.summary()
    );
}

#[test]
fn the_local_binding_refusal_never_becomes_a_decision() {
    // **It is not a rung.** A rung is tried after the stronger ones and before the weaker ones, and
    // the ladder's whole claim is "the first rung that returns any candidate decides" — which for
    // this class would be a claim that there *is* a candidate. So the test is not that the rule
    // outranks the others but that it prevents them from being asked at all: R1, R3, R4 and R5 all
    // have a route to this row and none of them may answer it.
    //
    // The fixture gives each of them one. The reference is in a file that imports nothing (R1 has no
    // binding), it is a bare name and not a qualified path (R3 has no scope), `out` is declared in
    // the *other* file (R4's same-file lookup finds nothing) and it is unique in the repository (R5
    // would answer, and wrongly). A rule placed after any of those would be caught by the `out` row
    // landing on `render.out`; a rule placed before all of them is caught by this test failing to
    // find it in `Resolved` or `Inferred` at all.
    let tree = local_binding_tree("never-a-decision");
    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let decided = relations_in(
        &store,
        &ResolutionState::Resolved {
            by: Evidence::NameOnly,
        },
    )
    .into_iter()
    .chain(relations_in(
        &store,
        &ResolutionState::Inferred {
            by: Evidence::NameOnly,
            basis: String::new(),
        },
    ))
    .chain(relations_in(
        &store,
        &ResolutionState::Ambiguous {
            candidates: Vec::new(),
        },
    ));
    // Every decided state, and every *class* of it — the chain above queries the store by state and
    // the store keys on the state alone, so the evidence named in the query is a placeholder. That
    // is what makes one pass over all three meaningful: a relation resolved by R5 is `Inferred`, and
    // one placed by an import is `Resolved`, so a rule that let either through is caught whichever
    // rung produced it.
    let rows: Vec<Relation> = decided.collect();
    let placed = rows
        .iter()
        .filter(|relation| relation.kind == RelationKind::References)
        .filter(|relation| relation.source.qualified_name() == "summarise")
        .filter(|relation| relation.target_name == "out")
        .map(state_of)
        .collect::<Vec<_>>();
    assert!(
        placed.is_empty(),
        "`out` was decided rather than refused, so a rung answered it: {placed:?}"
    );
}

#[test]
fn a_re_decision_reads_the_refusal_in_all_three_states_that_carry_the_class() {
    // The clause most likely to be got wrong, and the one a `Pending`-only read gets wrong without
    // any test noticing: the second pass over an already-decided row finds `None`, the ladder runs,
    // and the relation comes back placed. R5 is the rung that does it, and it does it *correctly as
    // far as it knows* — which is the point. There is one entity named `out` and the reference is
    // `Pending` on the first pass only by luck of ordering.
    //
    // So all three states carrying `Evidence::LocalBinding` are exercised, and each is fed to a
    // *scoped* pass rather than a full one, because that is the pass that re-decides rows which
    // already hold an answer (`reconsider_decided`). `Resolved` and `Inferred` are not states this
    // engine produces for this class — the head rule refuses before either could be reached — so
    // they are written by hand, which is the honest way to test a defensive arm: the row is one an
    // index from another build, or a future change, could hold.
    let tree = local_binding_tree("read-in-every-state");
    let mut store = tree.index_without_resolving();

    // Read **before** the first pass, because a pass leaves nothing `Pending` and this test needs a
    // row the extractor really wrote — its source, its kind and its span are the store's natural
    // key, and a hand-built row would be a relation the graph could never contain.
    let pending_state = ResolutionState::Pending {
        evidence: Evidence::NameOnly,
        basis: String::new(),
    };
    let extracted = relations_in(&store, &pending_state);
    let original = references_from(&extracted, "summarise", "out")[0].clone();
    assert_eq!(
        original.resolution.evidence_class(),
        Some("local_binding"),
        "the fixture must produce a row carrying the class, or the test is about nothing: {}",
        state_of(&original)
    );

    let first = resolve_all(&mut store, ResolutionOptions::default()).expect("first pass");
    assert_eq!(
        first.unresolved_by_reason.get("local_binding"),
        Some(&4),
        "the fixture must produce the refusal the test is about: {}",
        first.summary()
    );

    let local = Evidence::LocalBinding {
        binder: "let_declaration".to_owned(),
    };
    let target = id("src/report.rs", EntityKind::Parameter, "render.out");

    // Each state, handed to `resolve_paths` as a displaced edge so the scoped pass has it in scope
    // without a file having changed. **The spans are shifted**, because the store's natural key
    // includes the span: three rows sharing one key are one row, and the pass would de-duplicate
    // them before the ladder saw two of the three.
    let shifted = |offset: u32| {
        crate::model::Span::new(
            original.span.start_byte + offset,
            original.span.end_byte + offset,
            original.span.start_line,
            original.span.start_column + offset,
            original.span.end_line,
            original.span.end_column + offset,
        )
        .expect("span")
    };
    let displaced = vec![
        Relation {
            span: shifted(100),
            target: None,
            resolution: ResolutionState::Pending {
                evidence: local.clone(),
                basis: String::new(),
            },
            ..original.clone()
        },
        Relation {
            span: shifted(200),
            target: Some(target.clone()),
            resolution: ResolutionState::Resolved { by: local.clone() },
            ..original.clone()
        },
        Relation {
            span: shifted(300),
            target: Some(target.clone()),
            resolution: ResolutionState::Inferred {
                by: local,
                basis: String::new(),
            },
            ..original.clone()
        },
    ];

    let report = resolve_paths(&mut store, &[], &displaced, ResolutionOptions::default())
        .expect("a scoped pass over the displaced rows");
    assert_eq!(
        report.examined,
        3,
        "all three rows must have reached the ladder: {}",
        report.summary()
    );
    assert_eq!(
        report.unresolved_by_reason.get("local_binding"),
        Some(&3),
        "and all three must have been refused rather than placed: {}",
        report.summary()
    );

    // Read back through the store, so this is what the graph holds and not what `decide` returned. The
    // two rows that arrived carrying a target are the discriminating ones: a `Pending`-only read
    // leaves them in place, and this is where that shows up rather than in the report counter.
    let refused_state = ResolutionState::Unresolved {
        reason: UnresolvedReason::LocalBinding,
    };
    let after = relations_in(&store, &refused_state);
    for shift in [100u32, 200, 300] {
        let row = after
            .iter()
            .find(|relation| relation.span.start_byte == original.span.start_byte + shift)
            .unwrap_or_else(|| {
                panic!(
                    "the row at offset {shift} is not in the store as a refusal; the store holds \
                     {} local_binding rows, at bytes {:?}",
                    after.len(),
                    after
                        .iter()
                        .map(|relation| relation.span.start_byte)
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(
            row.target,
            None,
            "a row shifted by {shift} arrived carrying a target and kept it, so the class was read \
             from the wrong state and the ladder answered it: {}",
            state_of(row)
        );
    }
}

#[test]
fn a_refusal_handed_back_to_the_ladder_stays_a_refusal() {
    // The cost of the wire format, tested from the side that matters.
    //
    // `ResolutionState::Unresolved` carries no evidence, so the class that justified the refusal is
    // gone from the stored row by the time the next pass reads it. Handed the row, the ladder finds
    // nothing and R5 places `out` on `render.out` — the exact wrong edge the refusal exists to
    // remove, restored by the act of removing it. So the reason is read back, and this is the test
    // that says the refusal survives being re-decided.
    //
    // **Displaced rows are the only route that reaches it, and the test says so rather than
    // implying otherwise.** A scoped pass opens two doors — the outgoing edges of the paths it was
    // given, and the incoming edges of the entities they declare — and a refused row has no target,
    // so `Store::incoming` cannot match it and it is not re-decided at all. That is a property of
    // an unplaced row rather than of this rule, and it is why the test below hands the row over
    // explicitly rather than pretending a refresh would. A re-extraction of `src/lib.rs` is the
    // other route, and it re-emits the relation as `Pending` with the class on it.
    //
    // **What this cannot do is restore the binder.** `explain` reads `unresolved (local_binding)` and
    // the `let_declaration` that wrote the name is not in the row any more. That is a real loss,
    // taken deliberately: a payload on `Unresolved` would be a second wire shape for one field that
    // every index already holds in the other. So this test asserts the refusal holds and says
    // nothing about the binder being recoverable, because it is not.
    let tree = local_binding_tree("refusal-survives");
    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("first pass");

    let already_refused = ResolutionState::Unresolved {
        reason: UnresolvedReason::LocalBinding,
    };
    let held = relations_in(&store, &already_refused);
    let before = references_from(&held, "summarise", "out");
    assert!(
        before.iter().all(|row| row.target.is_none()),
        "every refusal starts unplaced, or re-deciding one proves nothing: {:?}",
        before.iter().map(|row| state_of(row)).collect::<Vec<_>>()
    );
    assert!(
        before
            .iter()
            .all(|row| row.resolution.evidence_class().is_none()),
        "and the refusal carries no evidence, which is the whole of the cost: {:?}",
        before
            .iter()
            .map(|row| row.resolution.evidence_class())
            .collect::<Vec<_>>()
    );
    let handed_back = before[0].clone();

    let report = resolve_paths(
        &mut store,
        &[],
        &[handed_back],
        ResolutionOptions::default(),
    )
    .expect("a scoped pass over the displaced row");
    assert_eq!(
        report.examined,
        1,
        "the row must have reached the ladder: {}",
        report.summary()
    );
    assert_eq!(
        report.unresolved_by_reason.get("local_binding"),
        Some(&1),
        "and it must have been refused again for the same reason, not placed by R5: {}",
        report.summary()
    );

    let after = relations_in(&store, &already_refused);
    let placed = references_from(&after, "summarise", "out")
        .into_iter()
        .filter(|row| row.target.is_some())
        .map(state_of)
        .collect::<Vec<_>>();
    assert!(
        placed.is_empty(),
        "the second pass placed `out` on an entity it had already refused: {placed:?}"
    );
}

// ---------------------------------------------------------------------------
// A candidate has to be in scope where the use is written
// ---------------------------------------------------------------------------
//
// Four tests, one per clause plus the pair that could have been one rule. Each is a
// source a reviewer can read, and each names the answer the ladder would have given
// before the rule as well as the one it gives after — because a rule whose damage is
// invisible is a rule whose damage is unbounded.

/// The fixture the first clause needs: an imported name a parameter shadows.
///
/// `describe` takes a parameter called `entry` and the file also imports a function
/// called `entry` from another crate. Inside `describe` the parameter is the one the
/// source means, in every language this engine reads, because a parameter list is
/// inside the body of the function it belongs to.
fn shadowed_import_tree(label: &str) -> TempTree {
    let tree = TempTree::new(label);
    tree.write("src/model.rs", "pub fn entry(value: u8) -> u8 {\n    value\n}\n");
    tree.write(
        "src/report.rs",
        "use crate::model::entry;\n\n\
         pub fn describe(entry: u8) -> u8 {\n    entry\n}\n\n\
         pub fn renamed() -> u8 {\n    entry(1)\n}\n",
    );
    tree
}

#[test]
fn a_declaration_in_the_source_own_scope_beats_the_import_it_shadows() {
    // The first clause, and the fixture is shaped so a rung that ignored it would look right.
    // `model.rs` declares exactly one entity named `entry`, so a repository-wide lookup agrees
    // with the parameter by accident; only the *import* rung can be shown to have been wrong.
    let tree = shadowed_import_tree("own-scope");
    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let shadowed = references_from(
        &relations_of(&store, RelationKind::References),
        "describe",
        "entry",
    );
    assert_eq!(
        shadowed.len(),
        1,
        "the fixture writes `entry` once in `describe` and once as its parameter's own \
         declaration, and only the body use is a reference"
    );
    assert_eq!(
        shadowed[0].target,
        Some(id("src/report.rs", EntityKind::Parameter, "describe.entry")),
        "the parameter is what the body means, and it is in the same file as the import that beat \
         it before: {}",
        state_of(shadowed[0])
    );
    assert_eq!(
        rung_name(&decided_by(shadowed[0])),
        "same_file",
        "and the rung that answered is the one that reads the source's own scope: {}",
        state_of(shadowed[0])
    );

    // The other half of the clause, and the case that makes it a *scope* rule rather than a
    // blunt one: the same import still answers everywhere the parameter is not in scope. A rule
    // that refused the import for the whole file would break this row.
    let elsewhere = references_from(
        &relations_of(&store, RelationKind::References),
        "renamed",
        "entry",
    );
    assert_eq!(
        elsewhere[0].target,
        Some(id("src/model.rs", EntityKind::Function, "entry")),
        "`renamed` has no `entry` of its own, so the import is the answer there: {}",
        state_of(elsewhere[0])
    );
}

#[test]
fn a_binding_of_another_declaration_is_not_in_scope_even_in_the_same_file() {
    // The second clause, and the sense in which same file is not the same scope.
    //
    // Two functions in one file each declare a parameter called `count`, and a third reads
    // `charge.count` through a parameter of its own. The field read is the answer, and a
    // same-file rung that offered the parameters would get it wrong for both reasons at once:
    // the name belongs to a different declaration, and a field is not a binding at all.
    let tree = TempTree::new("foreign-binding");
    tree.write(
        "src/model.rs",
        "pub struct Charge {\n    pub count: u8,\n}\n",
    );
    tree.write(
        "src/report.rs",
        "pub fn first(count: u8) -> u8 {\n    count\n}\n\n\
         pub fn second(count: u8) -> u8 {\n    count\n}\n\n\
         pub fn total(charge: &Charge) -> u8 {\n    charge.count\n}\n",
    );
    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let read = references_from(
        &relations_of(&store, RelationKind::References),
        "total",
        "count",
    );
    assert_eq!(
        read[0].target,
        Some(id("src/model.rs", EntityKind::Field, "Charge.count")),
        "`charge.count` is a field read, and both parameters called `count` are in the same file: {}",
        state_of(read[0])
    );

    // And the two parameters are still each function's own. The clause is not "a parameter is a
    // worse candidate" — it is "a parameter of a function this use is not written inside is not a
    // candidate", so both of these are unchanged by it.
    for (subject, qualified) in [("first", "first.count"), ("second", "second.count")] {
        let own = references_from(&relations_of(&store, RelationKind::References), subject, "count");
        assert_eq!(
            own[0].target,
            Some(id("src/report.rs", EntityKind::Parameter, qualified)),
            "`{subject}` reads its own parameter: {}",
            state_of(own[0])
        );
    }
}

#[test]
fn a_call_through_a_parameter_is_not_a_call_of_the_parameter() {
    // The third clause, and the only one that reads the relation's class — so it is the clause
    // most likely to be got wrong by a reader who assumes one rule fits both classes.
    //
    // `body` is a parameter of type `impl Fn() -> usize`. A reference to `body` is the parameter,
    // and a call of `body` invokes the value the parameter holds, which no entity in the index
    // denotes. Getting this backwards produces an edge saying a function calls its own parameter.
    let tree = TempTree::new("call-through-a-binding");
    tree.write(
        "src/lib.rs",
        "pub fn run(body: impl Fn() -> usize, limit: usize) -> usize {\n    \
         let mut last = 0;\n    for step in 0..limit {\n        last = body();\n    }\n    last\n}\n",
    );
    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "body");
    assert!(
        !call.target.is_some(),
        "a call through a parameter has no static target, and naming the parameter would say this \
         function invokes itself: {}",
        state_of(&call)
    );

    // The same name, referred to rather than invoked. This is the pair the two clauses differ on,
    // and asserting it is what stops the third clause being read as a relaxation of the first.
    let reference = references_from(
        &relations_of(&store, RelationKind::References),
        "run",
        "body",
    );
    assert_eq!(
        reference[0].target,
        Some(id("src/lib.rs", EntityKind::Parameter, "run.body")),
        "a reference to `body` is the parameter, and only the call has no static target: {}",
        state_of(reference[0])
    );
}

#[test]
fn a_declaration_in_an_enclosing_scope_answers_for_everything_inside_it() {
    // The shape that makes the rule a rule about scope rather than about a function's own
    // parameters: a field on a type, read from a method of that type and from a function that
    // takes one. Both are answered by the declaration the scope encloses, and neither falls
    // through to a repository-wide lookup.
    let tree = TempTree::new("enclosing-scope");
    tree.write(
        "src/lib.rs",
        "pub struct Charge {\n    pub count: u8,\n}\n\n\
         impl Charge {\n    pub fn count(&self) -> u8 {\n        self.count\n    }\n}\n\n\
         pub fn total(charge: &Charge) -> u8 {\n    charge.count\n}\n",
    );
    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    for subject in ["Charge.count", "total"] {
        let read = references_from(&relations_of(&store, RelationKind::References), subject, "count");
        assert_eq!(
            read[0].target,
            Some(id("src/lib.rs", EntityKind::Field, "Charge.count")),
            "`{subject}` reads a field of `Charge`, and the field is declared in a scope that \
             encloses it: {}",
            state_of(read[0])
        );
    }
}

// ---------------------------------------------------------------------------
// The evidence order
// ---------------------------------------------------------------------------

#[test]
fn a_cross_package_import_is_placed_by_the_module_table_and_by_nothing_else() {
    // The case R-007 exists for, and the one the path guess structurally cannot reach.
    //
    // `alpha`'s module `gateway` lives at `crates/alpha/src/gateway.rs`. The guess builds
    // candidate paths by anchoring on the importer's directory and climbing, joining the module's
    // segments with `/` and appending `.rs` — so from `crates/beta/src/` it will try
    // `crates/beta/alpha/gateway.rs`, `crates/alpha/gateway.rs`, and so on. It never produces
    // `crates/alpha/src/gateway.rs`, because the `src` directory is not in the module path and
    // nothing in the module name mentions it. So a cross-crate import is unplaceable by the guess
    // no matter how many anchors it tries, which is exactly why it was `no_candidate`.
    //
    // The table has no such difficulty: `alpha::gateway` *is* the qualified name the extractor
    // wrote, and one indexed seek finds it.
    let tree = TempTree::new("cross-package");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/alpha/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        1\n    }\n}\n",
    );
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "use alpha::gateway::Gateway;\n\npub fn go() -> u8 {\n    Gateway.send()\n}\n",
    );
    // A second package declaring the same type and the same method. This is the discriminator for
    // both halves of the test: with one `Gateway` in the repository a repository-wide name lookup
    // could reach the right answer by luck, so a rung that never ran would look identical to one
    // that did. Two of them, in two packages, can only be told apart by the path in the import.
    tree.write("crates/gamma/Cargo.toml", "[package]\nname = \"gamma\"\n");
    tree.write("crates/gamma/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/gamma/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        2\n    }\n}\n",
    );

    let mut store = tree.index_without_resolving();
    let options = ResolutionOptions {
        use_module_table: true,
        ..ResolutionOptions::default()
    };
    resolve_all(&mut store, options).expect("resolve with the module table on");
    let alpha = "crates/alpha/src/gateway.rs";
    let gamma = "crates/gamma/src/gateway.rs";

    let import = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "Gateway")
        .expect("the import of Gateway was extracted");
    // Asserted on the typed state rather than on the rendered string, so the assertion is about
    // the answer and not about a formatter's wording.
    //
    // **This expectation changed, and the change is the fix.** The answer used to be `Ambiguous`
    // with two candidates: the `struct Gateway`, and a *second* entity also named `Gateway` of kind
    // `Module`, which is the `impl Gateway` block. An impl block is emitted as an entity because
    // the walker's scope stack needs an id to anchor methods to, and that entity then competed for
    // the type's own name in every lookup (R-012). The import now lands on the struct the author
    // named, because a namespace is outranked by a symbol.
    assert_eq!(
        import.target,
        Some(id(alpha, EntityKind::Struct, "Gateway")),
        "the import names the struct, and a namespace that shares its name does not make it \
         ambiguous: {import:?}"
    );
    match &import.resolution {
        ResolutionState::Resolved { by } => assert_eq!(
            by.class(),
            "import_binding",
            "and the evidence is still the author's own import: {import:?}"
        ),
        other => panic!(
            "expected a resolved import, got {other:?}: {}",
            state_of(&import)
        ),
    }

    // The impl block is still indexed. Fixing the lookup was not a deletion: the methods inside it
    // are contained by that row, so removing it would leave a dangling edge the foreign key
    // rejects.
    let impl_block = id(alpha, EntityKind::Module, "Gateway");
    assert!(
        store.entity(&impl_block).expect("read it").is_some(),
        "the scope row the methods hang from is still in the index"
    );

    // The method call is now placed, and the rung that placed it is the import rather than the
    // receiver. It used to be `no_candidate` even though `send` is declared in
    // `crates/alpha/src/gateway.rs` and the receiver is written out in full.
    //
    // R-012 blamed the impl block's entity — that the receiver rung cannot commit to an owner
    // because `Gateway` is ambiguous between the struct and the block. The test
    // `a_receiver_resolves_when_its_type_is_in_the_callers_file_and_through_an_import_when_it_is_not`
    // measures that hypothesis and refutes it: with two owner candidates and the type in the
    // caller's file the call resolves, and with the *same* two candidates and the type in another
    // file it did not. What decided it was that the receiver rung looked for the owner **in the
    // caller's file only**, was terminal when it found nothing, and left R1 — the rung that knows
    // where an imported name came from — skipped for exactly the relations that needed it.
    //
    // So this assertion records the answer D-0036 produces rather than the one it removed. It was
    // written to fail the moment that was fixed, and it did; `gamma` is what keeps it honest now,
    // because a `Gateway.send` in two packages is a name a repository-wide search cannot place and
    // only the import can.
    let call = relations_of(&store, RelationKind::Calls)
        .into_iter()
        .find(|relation| relation.target_name == "send")
        .expect("the call to send was extracted");
    assert_eq!(
        call.target,
        Some(id(alpha, EntityKind::Method, "Gateway.send")),
        "the method call on a cross-package type is placed, in the package the import named: \
         {}",
        state_of(&call)
    );
    assert_eq!(
        rung_name(&decided_by(&call)),
        "import_binding",
        "and the rung that placed it is the one that knows where an imported name came from: {}",
        state_of(&call)
    );
    assert_ne!(
        call.target,
        Some(id(gamma, EntityKind::Method, "Gateway.send")),
        "not the same method in the other package: {}",
        state_of(&call)
    );
}

#[test]
fn the_module_table_switch_really_turns_the_table_off() {
    // Without this, "the two arms measured identically" would be unreadable: it would be a result
    // or a broken switch, and there would be no way to tell. So the switch is proved to do
    // something, on the same repository, in the same run.
    //
    // The counter-case is the same cross-package repository, with the table off. The import must
    // then be `no_candidate` — the exact defect the table fixes — which is what makes this a test
    // of the switch rather than of the resolver.
    let tree = TempTree::new("cross-package-off");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/alpha/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        1\n    }\n}\n",
    );
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "use alpha::gateway::Gateway;\n\npub fn go() -> u8 {\n    Gateway.send()\n}\n",
    );

    // A **second** package that also declares a `Gateway`. This is what makes the two arms
    // distinguishable at all, and it is worth being explicit about why.
    //
    // With one `Gateway` in the repository, the two arms could report the same thing and the
    // switch could be broken with nothing to show it. The package boundary is the discriminator:
    // the module table can only see the file it looked up, so it places the import inside `alpha`
    // and stops. The fallback builds candidate paths from the importer's own directory and cannot
    // construct `crates/gamma/src/gateway.rs` from `crates/beta/src/`, so the import is not placed
    // at all and the repository-wide rungs answer it — with no package boundary to respect.
    tree.write("crates/gamma/Cargo.toml", "[package]\nname = \"gamma\"\n");
    tree.write("crates/gamma/src/lib.rs", "pub mod gateway;\n");
    tree.write("crates/gamma/src/gateway.rs", "pub struct Gateway;\n");

    let store = tree.index_without_resolving();

    // The decision the arm reached, and the files it decided between. A single target is the table
    // working; a candidate list is the fallback searching the whole repository.
    let decide = |options: ResolutionOptions| -> (String, Vec<RepoPath>) {
        let mut scratch = Store::open(&tree.db, store.repo()).expect("a second handle");
        let _ = &mut scratch;
        // Resolve into a fresh copy of the same relations so the two arms are independent.
        let mut update = crate::store::IndexUpdate::empty();
        for relation in relations_of(&store, RelationKind::Imports) {
            let mut pending = relation.clone();
            let evidence = match &relation.resolution {
                ResolutionState::Pending { evidence, .. } => evidence.clone(),
                ResolutionState::Resolved { by } => by.clone(),
                ResolutionState::Inferred { by, .. } => by.clone(),
                ResolutionState::Ambiguous { .. } => Evidence::NameOnly,
                ResolutionState::Unresolved { .. } => Evidence::NameOnly,
            };
            pending.resolution = ResolutionState::Pending {
                evidence,
                basis: "reset for the second arm".to_owned(),
            };
            update = update.with_relation(pending);
        }
        scratch
            .apply_update(update)
            .expect("reset the relations to pending");
        resolve_all(&mut scratch, options).expect("resolve");
        let import = relations_of(&scratch, RelationKind::Imports)
            .into_iter()
            .find(|relation| relation.target_name == "Gateway")
            .expect("the import of Gateway");
        let paths: Vec<RepoPath> = match &import.target {
            Some(target) => vec![target.path().clone()],
            None => match &import.resolution {
                ResolutionState::Ambiguous { candidates } => {
                    candidates.iter().map(|id| id.path().clone()).collect()
                }
                other => panic!("the import must be decided one way or the other, got {other:?}"),
            },
        };
        (import.resolution.describe(), paths)
    };

    let (with_table, with_paths) = decide(ResolutionOptions {
        use_module_table: true,
        ..ResolutionOptions::default()
    });
    let (without_table, without_paths) = decide(ResolutionOptions {
        use_module_table: false,
        ..ResolutionOptions::default()
    });

    assert!(
        with_table.starts_with("resolved"),
        "with the table the import is placed, because `alpha::gateway` is a name the index holds \
         and one seek finds the file it means: {with_table}"
    );
    let alpha = RepoPath::new("crates/alpha/src/gateway.rs").expect("valid path");
    assert_eq!(
        with_paths,
        vec![alpha],
        "and it is placed inside the package the path named, and nowhere else: \
         {with_table} {with_paths:?}"
    );
    assert!(
        without_table.starts_with("ambiguous"),
        "without the table the import cannot be placed at all, so it falls through to the \
         repository-wide rungs: {without_table}"
    );
    assert_eq!(
        without_paths.len(),
        2,
        "and those find one `Gateway` in each of the two packages: {without_table} \
         {without_paths:?}"
    );
    assert!(
        without_paths
            .iter()
            .any(|path| path.as_str() == "crates/gamma/src/gateway.rs"),
        "gamma is exactly the wrong answer: {without_table} {without_paths:?}"
    );
    assert!(
        !with_paths
            .iter()
            .any(|path| path.as_str() == "crates/gamma/src/gateway.rs"),
        "so the table is what keeps the two packages apart: {with_paths:?}"
    );
}

#[test]
fn a_receiver_resolves_when_its_type_is_in_the_callers_file_and_through_an_import_when_it_is_not() {
    // R-012's hypothesis, measured rather than assumed.
    //
    // The hypothesis was that the receiver rung has to name the receiver's owner before it can
    // look for a method inside it, that `impl Gateway { .. }` is indexed as a *second* entity
    // named `Gateway`, and that the rung therefore cannot commit to an owner — so every method
    // call on every type with an `impl` block is unplaceable.
    //
    // It does not hold, and the reason is worth recording because it is a property of the rung
    // rather than a fact about one fixture: `via_receiver` collects its owners as **names**, not as
    // entity identities, so any number of entities sharing the receiver's name produce one owner
    // string and one search, and `decide_candidates` drops the duplicate identities that leaves
    // behind. The candidate count below is identical in both arms.
    //
    // **The two arms used to have opposite answers and now do not, and that is the fix.** The old
    // difference was not the collision — the count is the same either way — it was that the rung
    // read only the caller's file and refused when the owner was not there. D-0036 made the
    // refusal a decline, so the same call now resolves through the import that brought the name
    // in, and the rung that answers is R1 rather than R2. What the count still measures is
    // unchanged: the collision is real, it is present in both arms, and it is not what decides the
    // answer.
    let gateway = "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) {}\n}\n";

    // Arm one: the type, its `impl` block and the call are all in one file, and the receiver is
    // written exactly as the type's name.
    let near = TempTree::new("receiver-owner-in-file");
    let source = format!("{gateway}pub fn go() {{ Gateway.send() }}\n");
    near.write("src/lib.rs", &source);
    let mut near_store = near.index_without_resolving();
    let owners = near_store
        .entities_named("Gateway", 16)
        .expect("entities carrying the receiver's name");
    assert_eq!(
        owners.len(),
        2,
        "the collision R-012 describes is present: the struct and the impl block's own row measure \
         as two entities named `Gateway`: {owners:?}"
    );
    assert!(
        owners
            .iter()
            .any(|entity| entity.kind() == EntityKind::Struct),
        "and one of them is the type: {owners:?}"
    );

    resolve_all(&mut near_store, ResolutionOptions::default()).expect("resolve");
    let call = the_call(&near_store, "send");
    assert_eq!(
        call.resolution,
        ResolutionState::Resolved {
            by: Evidence::ReceiverType {
                receiver: "Gateway".to_owned()
            }
        },
        "two owner candidates and the method is placed anyway, because the rung compares names \
         rather than identities: {}",
        state_of(&call)
    );
    assert_eq!(
        call.target,
        Some(id("src/lib.rs", EntityKind::Method, "Gateway.send")),
        "and it lands on the method, which is qualified by its type: {}",
        state_of(&call)
    );

    // Arm two: the same call, with the receiver's type in another package. The candidate count is
    // still the same two, so the collision still cannot be what decides the answer.
    let far = TempTree::new("receiver-owner-elsewhere");
    far.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    far.write("crates/alpha/src/gateway.rs", gateway);
    far.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    far.write(
        "crates/beta/src/lib.rs",
        "use alpha::gateway::Gateway;\n\npub fn go() {\n    Gateway.send()\n}\n",
    );
    let mut far_store = far.index_without_resolving();
    let far_owners = far_store
        .entities_named("Gateway", 16)
        .expect("entities carrying the receiver's name");
    assert_eq!(
        far_owners.len(),
        2,
        "the same two entities carry the name, in the other package: {far_owners:?}"
    );

    resolve_all(&mut far_store, ResolutionOptions::default()).expect("resolve");
    let call = the_call(&far_store, "send");
    assert_eq!(
        call.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Method,
            "Gateway.send"
        )),
        "the identical candidate count, and the same method reached — this time through the \
         import that brought the name into the file: {}",
        state_of(&call)
    );
    assert_ne!(
        rung_name(&decided_by(&call)),
        rung_name(&Evidence::ReceiverType {
            receiver: "Gateway".to_owned()
        }),
        "the two arms are answered by two different rules, and the difference is visible rather \
         than implied: {}",
        state_of(&call)
    );
}

#[test]
fn a_name_shared_by_a_namespace_and_a_symbol_resolves_to_the_symbol() {
    // Audit B21 is what happens when something that is not a declaration wins a name lookup: a
    // well-formed answer about the wrong entity, or an ambiguity that is not uncertainty. The
    // file entity was the original case, and the module table and the `impl` row give it two more
    // that look identical from the store — two entities, one name, no way to tell which is a
    // declaration.
    //
    // The shape here is ordinary Rust and needs no module table to arise: a file that declares a
    // type, an `impl` block for it, and a second file that imports the type. The import names
    // `Gateway`; the index holds `struct Gateway` and the row the `impl Gateway { .. }` block
    // needs for its methods to hang from. Before the fix this came back `Ambiguous` between the
    // two, which is R-012's first measured row.
    let tree = TempTree::new("namespace-loses-to-symbol");
    tree.write(
        "src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        1\n    }\n}\n",
    );
    tree.write("src/app.rs", "use crate::gateway::Gateway;\nfn boot() {}\n");
    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let import = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "Gateway")
        .expect("the import was extracted");
    assert_eq!(
        import.target,
        Some(id("src/gateway.rs", EntityKind::Struct, "Gateway")),
        "the name means the struct, and the namespace that shares it is not a candidate: {}",
        state_of(&import)
    );
    match &import.resolution {
        ResolutionState::Resolved { by } => assert_eq!(
            by.class(),
            "import_binding",
            "and it is resolved by the author's own import rather than left ambiguous: {}",
            state_of(&import)
        ),
        other => panic!(
            "a name shared by a namespace and a symbol must decide, got {other:?}: {}",
            state_of(&import)
        ),
    }

    // The namespace is still in the index. Excluding it from a lookup is not the same as deleting
    // it, and a rule that started deleting rows would take the methods' containment with it.
    let impl_block = id("src/gateway.rs", EntityKind::Module, "Gateway");
    assert!(
        store.entity(&impl_block).expect("read it").is_some(),
        "the impl block's own row is still there: the methods are contained by it"
    );
    let method = id("src/gateway.rs", EntityKind::Method, "Gateway.send");
    assert!(
        store
            .incoming(&method, None, 8)
            .expect("read the containment")
            .iter()
            .any(|relation| relation.source == impl_block),
        "and the method is still contained by it, so the row is load-bearing rather than spare"
    );
}

#[test]
fn an_import_that_names_only_a_namespace_still_resolves() {
    // The other half of the rule, and the half that makes it a ranking rather than an exclusion.
    // `use payments::service;` names a module and nothing else, and it has to keep resolving: an
    // import the author wrote is the strongest evidence the resolver has, and dropping every
    // namespace candidate would trade one false ambiguity for a large class of `no_candidate`.
    let tree = TempTree::new("module-import-still-resolves");
    tree.write("src/payments/service.rs", "pub fn charge() {}\n");
    tree.write("src/app.rs", "use payments::service;\nfn boot() {}\n");
    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let import = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "service")
        .expect("the import was extracted");
    let service = "src/payments/service.rs";
    let module = id(service, EntityKind::Module, "src::payments::service");
    assert_eq!(
        import.target,
        Some(module.clone()),
        "nothing else carries the name, so the namespace stands: {}",
        state_of(&import)
    );
    assert_eq!(
        import.resolution,
        ResolutionState::Resolved {
            by: Evidence::ImportBinding {
                module: "payments::service".to_owned(),
                alias: None,
            }
        },
        "and the evidence is still the author's own import: {}",
        state_of(&import)
    );
    assert!(
        store.entity(&module).expect("read it").is_some(),
        "and the row it names is a real one: a module, in the file that declares it"
    );
}

#[test]
fn the_evidence_order_is_total_and_matches_the_documented_rung_order() {
    // The order is a claim the engine makes about its own output, and `peek explain` prints it.
    // If two classes could ever compare equal, "ordered by evidence strength" would be a phrase
    // rather than a rule.
    let ladder = [
        (Evidence::Containment, 100u8),
        (
            Evidence::ImportBinding {
                module: "a".to_owned(),
                alias: None,
            },
            90,
        ),
        (
            Evidence::ReceiverType {
                receiver: "a".to_owned(),
            },
            85,
        ),
        (Evidence::PathMatch, 80),
        (
            Evidence::QualifiedNameInScope {
                scope: "a".to_owned(),
            },
            70,
        ),
        (Evidence::SameFile, 50),
        (Evidence::UniqueName, 40),
        (Evidence::NameOnly, 10),
    ];

    for pair in ladder.windows(2) {
        assert!(
            pair[0].1 > pair[1].1,
            "{} ({}) must outrank {} ({}), or the rung order is not the evidence order",
            pair[0].0.class(),
            pair[0].1,
            pair[1].0.class(),
            pair[1].1,
        );
    }

    // Every class the resolver can produce names a rung, and no two classes share one. A rung
    // that quietly stopped firing would otherwise be a silent change in what the engine believes.
    let mut rungs: Vec<&'static str> = ladder
        .iter()
        .map(|(evidence, _)| rung_name(evidence))
        .collect();
    rungs.sort_unstable();
    let count = rungs.len();
    rungs.dedup();
    assert_eq!(
        rungs.len(),
        count,
        "two evidence classes map to one rung, so a decision could not name which rule fired"
    );

    // And the rule name is the stored class, not a second spelling that can drift from it.
    for (evidence, _) in &ladder {
        assert_eq!(
            rule_name(evidence),
            evidence.class(),
            "the rule name and the stored evidence class must be the same string for {evidence:?}"
        );
    }
}

#[test]
fn an_import_that_names_a_module_rather_than_an_item_binds_to_that_modules_file() {
    // `use payments::service as svc;` brings a *module* into scope, not a symbol, so the thing
    // the import relation points at is the module's file. The engine Peek replaces had no alias
    // field on its relation type at all (audit B5), so `use x as y` and `use y` produced the same
    // record and a module import became a repo-global name lookup (audit B6) — a false positive
    // indistinguishable from a real call edge.
    let tree = TempTree::new("module-import");
    tree.write("src/payments/service.rs", "pub fn charge() {}\n");
    tree.write(
        "src/app.rs",
        "use payments::service as svc;\nfn boot() {}\n",
    );

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let imports: Vec<_> = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .filter(|relation| relation.target_name == "svc")
        .collect();
    assert_eq!(
        imports.len(),
        1,
        "expected one import of `svc`, found {imports:?}"
    );
    assert_eq!(
        imports[0].target,
        Some(id(
            "src/payments/service.rs",
            EntityKind::File,
            "service.rs"
        )),
        "a module import binds to the module's file: {}",
        state_of(&imports[0])
    );
    match &imports[0].resolution {
        ResolutionState::Resolved { by } => assert_eq!(
            by.class(),
            "import_binding",
            "the evidence must still be the author's own import: {}",
            state_of(&imports[0])
        ),
        other => panic!(
            "a module import the index can locate must resolve, got {other:?}: {}",
            state_of(&imports[0])
        ),
    }
}

// ---------------------------------------------------------------------------
// The required behaviours
// ---------------------------------------------------------------------------

#[test]
fn a_call_to_a_function_in_the_same_file_resolves() {
    // The plainest case, and the one that must work: the definition is in the file the call is
    // written in, so nothing has to be inferred.
    let tree = TempTree::new("same-file");
    tree.write("src/lib.rs", "fn helper() {}\nfn main() { helper(); }\n");

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "helper");
    assert_eq!(
        call.resolution,
        ResolutionState::Resolved {
            by: Evidence::SameFile
        },
        "a call to a definition in the same file is proven, not inferred: {}",
        state_of(&call)
    );
    assert_eq!(
        call.target,
        Some(id("src/lib.rs", EntityKind::Function, "helper")),
        "and it points at that definition: {}",
        state_of(&call)
    );
}

#[test]
fn a_call_bound_by_an_import_in_another_file_resolves_across_files() {
    // The engine Peek replaces recorded `use crate::model::Edge` as a reference to the *module*
    // `crate::model` and then looked `Edge` up in a repo-global name table (audit B5, B6). The
    // alias field did not exist on its relation type at all, so the mapping was destroyed at
    // extraction time.
    let tree = TempTree::new("import-bound");
    tree.write("src/payments.rs", "pub fn charge(amount: u32) {}\n");
    tree.write(
        "src/app.rs",
        "use payments::charge;\nfn boot() { charge(1); }\n",
    );

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "charge");
    assert_eq!(
        call.target,
        Some(id("src/payments.rs", EntityKind::Function, "charge")),
        "the call must follow the import to the other file: {}",
        state_of(&call)
    );
    match &call.resolution {
        ResolutionState::Resolved { by } => {
            assert_eq!(
                by.class(),
                "import_binding",
                "an import the author wrote is the strongest evidence available: {}",
                state_of(&call)
            );
        }
        other => panic!(
            "expected a resolved call, got {other:?}: {}",
            state_of(&call)
        ),
    }

    // And the import relation itself is decided, not left pending.
    let imports: Vec<_> = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .filter(|relation| relation.target_name == "charge")
        .collect();
    assert_eq!(
        imports.len(),
        1,
        "expected one import of `charge`, found {imports:?}"
    );
    assert_eq!(
        imports[0].target,
        Some(id("src/payments.rs", EntityKind::Function, "charge")),
        "the import itself must name the entity it brought in: {}",
        state_of(&imports[0])
    );
}

#[test]
fn two_symbols_with_the_same_name_in_different_files_are_ambiguous_and_neither_is_chosen() {
    // Audit B1, B3, B23: the "unique name" tier was dead code because the builder pushed a name
    // and its qualified form into the same key, so `symbols.len() >= 2` always. Every surviving
    // tier was "same file" or "first in lexicographic id order" — deterministic, arbitrary, and
    // silently wrong in a monorepo. This is the case that must be a result rather than a pick.
    let tree = TempTree::new("ambiguous");
    tree.write("src/one.rs", "pub fn charge() {}\n");
    tree.write("src/two.rs", "pub fn charge() {}\n");
    tree.write("src/driver.rs", "fn go() { charge(); }\n");

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "charge");
    assert_eq!(
        call.target,
        None,
        "an ambiguous relation must name no target at all: {}",
        state_of(&call)
    );
    let candidates = match &call.resolution {
        ResolutionState::Ambiguous { candidates } => candidates.clone(),
        other => panic!(
            "expected an ambiguous call, got {other:?}: {}",
            state_of(&call)
        ),
    };
    assert_eq!(
        candidates.len(),
        2,
        "both definitions must be reported: {candidates:?}"
    );
    assert!(candidates.contains(&id("src/one.rs", EntityKind::Function, "charge")));
    assert!(candidates.contains(&id("src/two.rs", EntityKind::Function, "charge")));

    // The candidates are retrievable, in the stored order, through the documented query.
    let stored = store
        .ambiguous_candidates(&call.source, RelationKind::Calls, "charge")
        .expect("read the candidate list back");
    assert_eq!(
        stored, candidates,
        "the candidate list must survive persistence in the order it was written"
    );
}

#[test]
fn a_name_nothing_in_the_repository_carries_is_unresolved_with_a_reason_and_still_indexed() {
    // Audit B9: `let Some(target) = … else { continue }` meant a relation that resolved to
    // nothing left no row, no counter and no log. The unresolved bucket has to be a queryable,
    // countable result, because "Peek has no idea" and "Peek believes this" are different facts
    // and only one of them can be silently wrong.
    let tree = TempTree::new("unknown-name");
    tree.write("src/lib.rs", "fn boot() { nowhere_to_be_found(); }\n");

    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "nowhere_to_be_found");
    assert_eq!(
        call.resolution,
        ResolutionState::Unresolved {
            reason: UnresolvedReason::NoCandidate
        },
        "an unknown bare name has no candidate: {}",
        state_of(&call)
    );
    assert_eq!(
        call.target,
        None,
        "and names no target: {}",
        state_of(&call)
    );

    // Still in the index, and countable by state.
    let unresolved = store
        .relations_in_state(
            &ResolutionState::Unresolved {
                reason: UnresolvedReason::NoCandidate,
            },
            64,
        )
        .expect("read the unresolved bucket");
    assert!(
        unresolved
            .iter()
            .any(|relation| relation.target_name == "nowhere_to_be_found"),
        "the relation must still be indexed: {unresolved:?}"
    );
    let counted = report
        .unresolved_by_reason
        .get("no_candidate")
        .copied()
        .unwrap_or(0);
    assert!(
        counted >= 1,
        "the report must count the reason, not just the total: {}",
        report.summary()
    );
    assert_eq!(
        store.stats().expect("stats").orphan_relations,
        0,
        "an unresolved relation names no entity, so it cannot be a dangling edge"
    );
}

#[test]
fn a_qualified_name_the_repository_does_not_contain_is_reported_as_external() {
    // `impl std::fmt::Debug for MyType` names a standard-library trait. Reporting that as "no
    // candidate found" would make the unresolved bucket a number nobody can act on; the thing
    // exists, it is simply outside the indexed tree, which is what `External` is for.
    let tree = TempTree::new("external");
    tree.write(
        "src/lib.rs",
        "struct MyType;\nimpl std::fmt::Debug for MyType {}\n",
    );

    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let implements: Vec<_> = relations_of(&store, RelationKind::Implements)
        .into_iter()
        .filter(|relation| relation.target_name == "std::fmt::Debug")
        .collect();
    assert_eq!(
        implements.len(),
        1,
        "expected one impl edge: {implements:?}"
    );
    assert_eq!(
        implements[0].resolution,
        ResolutionState::Unresolved {
            reason: UnresolvedReason::External
        },
        "a standard-library trait is external, not missing: {}",
        state_of(&implements[0])
    );
    let external = report
        .unresolved_by_reason
        .get("external")
        .copied()
        .unwrap_or(0);
    assert!(
        external >= 1,
        "and the report says so: {}",
        report.summary()
    );
}

#[test]
fn a_receiver_never_binds_to_an_unrelated_function_with_the_same_name() {
    // Audit B3, the worst of the resolution defects: `obj.method()` bound to *any symbol in the
    // repository* named `method` unless the caller's own file declared one — and the same-file
    // match actively overrode the receiver, so `Foo.render()` could bind to `Bar.render` in the
    // same file. A receiver is evidence about which entity, and a receiver with no identifiable
    // owner has to stay unresolved.
    let tree = TempTree::new("receiver-not-a-target");
    tree.write(
        "src/service.rs",
        "pub struct Service;\nimpl Service { pub fn charge(&self) {} }\n\
         pub fn charge() {}\n",
    );
    tree.write("src/app.rs", "fn boot() { let s = Service; s.charge(); }\n");

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "charge");
    assert_eq!(
        call.target,
        None,
        "an unidentifiable receiver must bind to nothing: {}",
        state_of(&call)
    );
    assert_eq!(
        call.resolution,
        ResolutionState::Unresolved {
            reason: UnresolvedReason::NoCandidate
        },
        "and must say why: {}",
        state_of(&call)
    );
    // Specifically not the free function of the same name in the other file, and not the method.
    for wrong in [
        id("src/service.rs", EntityKind::Function, "charge"),
        id("src/service.rs", EntityKind::Method, "Service.charge"),
    ] {
        assert_ne!(
            call.target,
            Some(wrong.clone()),
            "the receiver bound to {wrong:?} it has no evidence for: {}",
            state_of(&call)
        );
    }
}

#[test]
fn a_receiver_that_names_a_type_in_its_own_file_resolves_to_that_type_only() {
    // The positive half of the previous test, and the reason R2 is worth having: `self.charge()`
    // inside `impl Service` is exactly determined by the source's own qualified name, with no
    // inference and no repository-wide search.
    let tree = TempTree::new("receiver-resolves");
    tree.write(
        "src/lib.rs",
        "struct Service;\nimpl Service { fn charge(&self) { self.retry(); } \
         fn retry(&self) {} }\n",
    );

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "retry");
    assert_eq!(
        call.resolution,
        ResolutionState::Resolved {
            by: Evidence::ReceiverType {
                receiver: "self".to_owned()
            }
        },
        "`self.retry()` inside `Service` is proven by the enclosing scope: {}",
        state_of(&call)
    );
    assert_eq!(
        call.target,
        Some(id("src/lib.rs", EntityKind::Method, "Service.retry")),
        "and it points at that method: {}",
        state_of(&call)
    );
}

#[test]
fn a_method_on_a_type_imported_from_another_file_resolves_to_that_method_of_that_type() {
    // The defect, at its narrowest and with nothing else in the fixture to explain it: `Gateway`
    // reaches `beta` through a `use`, `Gateway.send()` is a method call on it, and `send` is
    // declared in another package.
    //
    // Every fixture below is load-bearing, and each one removes a way the answer could have been
    // right for the wrong reason.
    //
    // * **A second package with the same type and the same method.** A repository-wide name lookup
    //   cannot tell `alpha`'s `send` from `gamma`'s, so an implementation that searched by name
    //   would report an ambiguity rather than a target. Only the path in the import places it.
    // * **A free function called `send` in the caller's own file.** This is the shape that turns a
    //   fix into a regression: a rung that falls through to the repository-wide rungs, or to the
    //   same-file rung, lands there instead, and the edge looks plausible. The assertion below
    //   names that function so the test fails on it rather than passing beside it.
    // * **The target is the `Method` whose qualified name is `Gateway.send`.** A method is
    //   `Type.method`; an implementation that resolved the name `send` to a bare function would
    //   satisfy "something resolved" and lose the distinction the graph exists to carry.
    let tree = TempTree::new("imported-method");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/alpha/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        1\n    }\n}\n",
    );
    tree.write("crates/gamma/Cargo.toml", "[package]\nname = \"gamma\"\n");
    tree.write("crates/gamma/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/gamma/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        2\n    }\n}\n",
    );
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "use alpha::gateway::Gateway;\n\npub fn send() -> u8 {\n    0\n}\n\n\
         pub fn go() -> u8 {\n    Gateway.send()\n}\n",
    );

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "send");
    assert_eq!(
        call.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Method,
            "Gateway.send"
        )),
        "the method, on the type the import named, in the package the path named: {}",
        state_of(&call)
    );
    assert_eq!(
        rung_name(&decided_by(&call)),
        "import_binding",
        "and the rule that answered is the one that knows where an imported name came from: {}",
        state_of(&call)
    );
    for wrong in [
        id(
            "crates/gamma/src/gateway.rs",
            EntityKind::Method,
            "Gateway.send",
        ),
        id("crates/beta/src/lib.rs", EntityKind::Function, "send"),
    ] {
        assert_ne!(
            call.target,
            Some(wrong.clone()),
            "the call landed on {wrong:?}, which nothing in `beta` says it means: {}",
            state_of(&call)
        );
    }
}

#[test]
fn the_rung_and_the_evidence_class_are_both_readable_from_the_stored_state() {
    // `peek explain` reports the evidence class verbatim from the stored state and names the rung
    // through [`rung_name`], so those two strings are the whole of what a caller can learn about how
    // an edge was decided. Both are asserted here from the index rather than from the resolver's
    // internals, because a test on the rung that fired would keep passing while `explain` reported
    // something else.
    //
    // Two shapes in one fixture, and the point is that they are told apart: the same call syntax,
    // one with the type declared in the caller's file and one with it imported, must not produce the
    // same class. If they did, a consumer could not tell a proof by declaration from a proof by
    // import, and the ladder's one deviation from its own order would be invisible.
    let tree = TempTree::new("which-rung-answered");
    tree.write(
        "src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) {}\n}\n",
    );
    tree.write(
        "src/app.rs",
        "use crate::gateway::Gateway;\n\npub fn go() {\n    Gateway.send()\n}\n",
    );
    tree.write(
        "src/local.rs",
        "pub struct Local;\nimpl Local {\n    pub fn stop(&self) {}\n}\n\n\
         pub fn drive() {\n    Local.stop()\n}\n",
    );

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let imported = the_call(&store, "send");
    let local = the_call(&store, "stop");

    assert_eq!(
        imported.resolution.evidence_class(),
        Some("import_binding"),
        "the imported receiver is reported by the class that says how the name arrived: {}",
        state_of(&imported)
    );
    assert_eq!(
        local.resolution.evidence_class(),
        Some("receiver_type"),
        "and the declared one by the class that says the owner was read out of this file: {}",
        state_of(&local)
    );
    assert_eq!(
        rung_name(&decided_by(&imported)),
        "import_binding",
        "so each names its own rung, and neither names the other's"
    );
    assert_eq!(rung_name(&decided_by(&local)), "receiver_owner");
    assert_ne!(
        imported.resolution.evidence_class(),
        local.resolution.evidence_class(),
        "the two shapes must not be indistinguishable to a consumer: {} vs {}",
        state_of(&imported),
        state_of(&local)
    );
}

#[test]
fn an_impl_block_in_the_callers_file_is_not_an_owner_and_the_import_still_answers() {
    // The same defect one step to the side, and it is ordinary Rust rather than a corner.
    //
    // `impl Gateway { .. }` is indexed as a `Module` named `Gateway` because the walker's scope
    // stack needs a row to anchor the block's methods to. So a file that writes
    // `use alpha::gateway::Gateway;` *and* carries an inherent `impl Gateway` holds a `Gateway` that
    // declares nothing at all — the type arrived by import, and the row is the block's own scope.
    //
    // Reading that row as an owner is what made this shape unplaceable even after the decline was
    // fixed, because the rung then had an owner, found nothing inside it, and refused. The rule
    // that fixes it is the one this file already applies to candidates: a namespace is outranked
    // by a symbol, so a name only a namespace carries in this file is not an owner *here*.
    //
    // The fixture is `gamma`-shaped as well, so a name search could not have reached the right
    // answer by itself.
    let tree = TempTree::new("impl-block-is-not-an-owner");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/alpha/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        1\n    }\n}\n",
    );
    tree.write("crates/gamma/Cargo.toml", "[package]\nname = \"gamma\"\n");
    tree.write("crates/gamma/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/gamma/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        2\n    }\n}\n",
    );
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "use alpha::gateway::Gateway;\n\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        \
         self.receive()\n    }\n\n    pub fn receive(&self) -> u8 {\n        3\n    }\n}\n\n\
         pub fn go() -> u8 {\n    Gateway.send()\n}\n",
    );

    let mut store = tree.index_without_resolving();
    // The fixture has to contain what it claims: the block's scope row, and no declaration of the
    // type in this file. Without this the test would pass for a reason it does not name.
    let local = store
        .entities_in_file(
            &RepoPath::new("crates/beta/src/lib.rs").expect("valid path"),
            64,
        )
        .expect("the caller's entities");
    let owners: Vec<&str> = local
        .iter()
        .filter(|entity| entity.name == "Gateway")
        .map(|entity| entity.kind().as_str())
        .collect();
    assert_eq!(
        owners,
        vec!["module"],
        "the only `Gateway` in the caller's file is the impl block's own row: {local:?}"
    );

    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "send");
    assert_eq!(
        call.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Method,
            "Gateway.send"
        )),
        "the block's scope row is not an owner, so the decline stands and the import answers: {}",
        state_of(&call)
    );
    // And the same method name, written on a receiver that *is* declared here, stays local. One
    // name, two call sites, two files, two answers — which is what a receiver is for.
    let local_call = the_call(&store, "receive");
    assert_eq!(
        local_call.target,
        Some(id(
            "crates/beta/src/lib.rs",
            EntityKind::Method,
            "Gateway.receive"
        )),
        "a method this file declares is reached from the enclosing scope, not from the import: {}",
        state_of(&local_call)
    );
}

#[test]
fn a_re_decision_walks_the_same_ladder_and_keeps_the_rung_that_placed_it() {
    // A second defect, found while tracing this one, and it would have quietly undone the first.
    //
    // `resolve_paths` does not only decide what is pending: it re-decides every edge pointing *into*
    // a changed file, and a relation it re-decides is one that already holds an answer. The receiver
    // and the scope were read out of a `Pending` pattern, so on the second pass they were `None`, the
    // relation walked a shorter ladder, and the answer changed:
    //
    // * `Gateway.send()` through an import would lose the receiver, find no `send` in the caller's
    //   own file, and land on the repository-wide rung — which, with two packages declaring the
    //   method, is an **ambiguity between them**. A proven edge becomes an undecided one.
    // * `alpha::gateway::charge()` would lose its scope the same way, with the same outcome.
    //
    // Both targets are already stored in the evidence class, so both rungs read it in whatever state
    // the relation is in. The evidence is what D-0003 put in the schema for, and this is the test
    // that says so.
    //
    // The last assertion is the one that makes it a measurement rather than a claim: a re-decision
    // that changes nothing must write nothing and commit nothing, which is the property
    // `resolving_an_unchanged_index_is_a_no_op_that_moves_no_generation` states for a whole pass.
    let tree = TempTree::new("re-decision-keeps-the-rung");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/alpha/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        1\n    }\n}\n\n\
         pub fn charge() -> u8 {\n    1\n}\n",
    );
    tree.write("crates/gamma/Cargo.toml", "[package]\nname = \"gamma\"\n");
    tree.write("crates/gamma/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/gamma/src/gateway.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        2\n    }\n}\n\n\
         pub fn charge() -> u8 {\n    2\n}\n",
    );
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "use alpha::gateway::Gateway;\n\npub fn go() -> u8 {\n    Gateway.send()\n}\n\n\
         pub fn also() -> u8 {\n    alpha::gateway::charge()\n}\n",
    );

    let mut store = tree.index_without_resolving();
    let first = resolve_all(&mut store, ResolutionOptions::default()).expect("first pass");
    assert!(
        first.committed,
        "the fixture must produce decisions, or the re-decision proves nothing: {}",
        first.summary()
    );
    let receiver_call = the_call(&store, "send");
    let scoped_call = the_call(&store, "charge");
    assert_eq!(
        receiver_call.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Method,
            "Gateway.send"
        )),
        "the imported receiver resolves on the first pass: {}",
        state_of(&receiver_call)
    );
    assert_eq!(
        scoped_call.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Function,
            "charge"
        )),
        "and so does the fully qualified call: {}",
        state_of(&scoped_call)
    );

    // Now touch the file both targets live in. Nothing about the call sites changed, so nothing
    // about their answers should either.
    let report = resolve_paths(
        &mut store,
        &[
            RepoPath::new("crates/alpha/src/gateway.rs").expect("valid path"),
            RepoPath::new("crates/gamma/src/gateway.rs").expect("valid path"),
        ],
        &[],
        ResolutionOptions::default(),
    )
    .expect("a scoped pass over the declaring file");
    assert!(
        report.examined > 0,
        "the pass must have reached the edges pointing into those files: {}",
        report.summary()
    );
    assert_eq!(
        report.relations_written,
        0,
        "and must have found nothing to change, or a second ladder answered the same question \
         differently: {}",
        report.summary()
    );

    for (name, expected) in [("send", "Gateway.send"), ("charge", "charge")] {
        let call = the_call(&store, name);
        assert_eq!(
            call.target.as_ref().map(|target| target.qualified_name()),
            Some(expected),
            "`{name}` moved on the second pass, which means it was answered by a different rung: {}",
            state_of(&call)
        );
    }
    assert_eq!(
        rung_name(&decided_by(&the_call(&store, "send"))),
        "import_binding",
        "and the receiver call still carries the rung that placed it"
    );
    assert_eq!(
        rung_name(&decided_by(&the_call(&store, "charge"))),
        "scope_qualified_name",
        "as does the scoped one"
    );
}

#[test]
fn a_receiver_the_import_rung_cannot_place_still_refuses() {
    // The half of the ladder that must not move, and the one a fix like this most easily throws
    // away. The receiver rung now declines instead of refusing, and the rung that answers next is
    // the import one, so the question this pins is whether that rung's answer can be "some function
    // with the same name".
    //
    // It cannot, and the reason is in the fixture: `Service` **is** imported, so the import rung
    // runs and finds nothing it can place, because the receiver is `s` — a local variable, and the
    // index records no type for a local. Reaching the import rung and reaching a decision are two
    // different things, and the second is not reachable here by any route the ladder has.
    //
    // The two things it must not bind to are both in the fixture on purpose: the method
    // `Service.charge`, which is the right method on the wrong evidence, and the free `charge`
    // beside it, which is what the engine Peek replaces did to every receiver call (audit B3).
    let tree = TempTree::new("receiver-refuses-still");
    tree.write(
        "src/service.rs",
        "pub struct Service;\nimpl Service { pub fn charge(&self) {} }\npub fn charge() {}\n",
    );
    tree.write(
        "src/app.rs",
        "use crate::service::Service;\n\npub fn boot() {\n    let s = Service;\n    s.charge();\n}\n",
    );

    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "charge");
    assert_eq!(
        call.resolution,
        ResolutionState::Unresolved {
            reason: UnresolvedReason::NoCandidate
        },
        "a receiver the caller does not name a type by must still refuse: {}",
        state_of(&call)
    );
    assert_eq!(call.target, None, "and name no target: {}", state_of(&call));
    for wrong in [
        id("src/service.rs", EntityKind::Method, "Service.charge"),
        id("src/service.rs", EntityKind::Function, "charge"),
    ] {
        assert_ne!(
            call.target,
            Some(wrong.clone()),
            "the receiver bound to {wrong:?}, which nothing in `app.rs` says it means: {}",
            state_of(&call)
        );
    }
    // Counted as the refusal it is, so a caller reading the report sees the bucket and not a zero.
    assert!(
        report
            .unresolved_by_reason
            .get("no_candidate")
            .copied()
            .unwrap_or(0)
            >= 1,
        "and the pass must count it: {}",
        report.summary()
    );
}

#[test]
fn a_declaration_in_the_callers_file_is_not_overridden_by_an_import_of_the_same_name() {
    // The shadowing question, and the answer is pinned here rather than argued in prose: the ladder
    // is a chain, not a race. When the receiver rung finds an owner it owns the answer, and the
    // import rung is not asked to confirm it — not even to confirm it agrees.
    //
    // The fixture is a file **the compiler rejects**, which is measured rather than remembered:
    // rustc 1.98.1 on this shape gives `error[E0255]: the name `Gateway` is defined multiple
    // times`, alongside `warning: unused import`. So in Rust source that builds, a local
    // declaration and an import of one name cannot both be in a module, and R2 and R1 can never
    // hold a candidate for the same receiver at once. That is why the ladder is a chain and not a
    // race — the race has nothing to race over.
    //
    // The rule is pinned here anyway, for two reasons. Peek indexes a working tree rather than a
    // build artefact, and a file being edited is the ordinary state of a repository, so this shape
    // is reachable input; and the ordering rule is a whole-file rule, so a language where the
    // collision is legal would answer the same way under it.
    //
    // The last assertion is the one that carries the weight. A design in which the import rung
    // runs for confirmation, or wins on agreement, would produce a *different target* here and
    // would report `import_binding` rather than `receiver_owner`. Both assertions fail for it.
    let tree = TempTree::new("shadowing");
    tree.write(
        "src/remote.rs",
        "pub struct Gateway;\nimpl Gateway {\n    pub fn send(&self) -> u8 {\n        1\n    }\n}\n",
    );
    tree.write(
        "src/app.rs",
        "use crate::remote::Gateway;\n\npub struct Gateway;\n\n\
         impl Gateway {\n    pub fn send(&self) -> u8 {\n        2\n    }\n}\n\n\
         pub fn go() -> u8 {\n    Gateway.send()\n}\n",
    );

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "send");
    assert_eq!(
        call.target,
        Some(id("src/app.rs", EntityKind::Method, "Gateway.send")),
        "the declaration in this file is the owner, and the import does not displace it: {}",
        state_of(&call)
    );
    assert_ne!(
        call.target,
        Some(id("src/remote.rs", EntityKind::Method, "Gateway.send")),
        "and the imported type of the same name is not the answer either: {}",
        state_of(&call)
    );
    assert_eq!(
        rung_name(&decided_by(&call)),
        "receiver_owner",
        "the rung that answered is the receiver's own, which is only true if the import rung was \
         never asked to answer at all: {}",
        state_of(&call)
    );
}

#[test]
fn a_receiver_matched_while_ignoring_letter_case_is_a_claim_and_is_labelled_as_one() {
    // The one case-fold in the whole resolver, and it is deliberate. A local called `service` and
    // the type `Service` are the same thing, but the match is a guess — so it is recorded as
    // `Inferred` with a basis that says the case was ignored, never as `Resolved`. An unproven
    // match reported as a proof is exactly the confidently-wrong edge D-0003 exists to prevent.
    let tree = TempTree::new("case-folded-receiver");
    tree.write(
        "src/lib.rs",
        "struct Service;\nimpl Service { fn retry(&self) {} }\n\
         fn run() { let service = Service; service.retry(); }\n",
    );

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "retry");
    match &call.resolution {
        ResolutionState::Inferred { by, basis } => {
            assert_eq!(
                by.class(),
                "receiver_type",
                "the evidence class still names the rule that fired: {}",
                state_of(&call)
            );
            assert!(
                basis.contains("ignoring letter case"),
                "the basis must state that the case was ignored, or a consumer cannot discount \
                 the claim; got {basis:?}"
            );
        }
        other => {
            panic!("expected an inferred call, got {other:?}: a case-folded match is not a proof",)
        }
    }
    assert_eq!(
        call.target,
        Some(id("src/lib.rs", EntityKind::Method, "Service.retry")),
        "and it does name the method it inferred: {}",
        state_of(&call)
    );
}

#[test]
fn a_name_matched_only_by_a_repository_wide_uniqueness_check_is_inferred_not_resolved() {
    // Uniqueness is a fact about the index, not a statement about the author's intent. The
    // distinction is what `Inferred` exists for: the target is known, but how it was known has to
    // be written down.
    let tree = TempTree::new("unique-name");
    tree.write("src/payments.rs", "pub fn charge() {}\n");
    tree.write("src/driver.rs", "fn go() { charge(); }\n");

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "charge");
    match &call.resolution {
        ResolutionState::Inferred { by, basis } => {
            assert_eq!(
                by.class(),
                "unique_name",
                "a name that is unique in the repository is uniqueness evidence: {}",
                state_of(&call)
            );
            assert!(
                !basis.is_empty(),
                "an inference with no basis is a confident guess, which is the thing this \
                 project exists to remove"
            );
        }
        other => panic!(
            "expected an inferred call, got {other:?}: a name alone is never a proof: {}",
            state_of(&call)
        ),
    }
    assert_eq!(
        call.target,
        Some(id("src/payments.rs", EntityKind::Function, "charge")),
        "but the target is still recorded: {}",
        state_of(&call)
    );
}

// ---------------------------------------------------------------------------
// The pass as a whole
// ---------------------------------------------------------------------------

#[test]
fn a_full_build_resolves_its_own_relations_in_a_second_reportable_pass() {
    // The decision under test is that resolution is a *second pass*, not an inline step. It is
    // only worth anything if it is observable, so the report has to carry it and the summary has
    // to name it.
    let tree = TempTree::new("full-build");
    tree.write(
        "src/lib.rs",
        "struct Service;\nimpl Service { fn retry(&self) {} }\n\
         fn main() { let s = Service; s.retry(); }\n",
    );

    let mut store = Store::open(
        &tree.db,
        &RepoId::discover(tree.path()).expect("repository id"),
    )
    .expect("open the store");
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");

    let resolution = outcome
        .report()
        .resolution
        .clone()
        .expect("a full build must run the resolution pass");
    assert!(
        resolution.committed,
        "the pass decided something: {}",
        resolution.summary()
    );
    assert!(
        resolution.examined > 0,
        "and it examined something: {}",
        resolution.summary()
    );
    assert!(
        !resolution.pending_remaining,
        "a full build must leave nothing pending: {}",
        resolution.summary()
    );
    assert!(
        outcome.report().summary().contains("resolution pass 2"),
        "the index report must name the pass so `peek status` can show it: {}",
        outcome.report().summary()
    );
    // Two commits, not one, and the generation says so.
    assert!(
        resolution.generation > 1,
        "resolution is a second commit, so the generation must have advanced twice; it is {}",
        resolution.generation
    );
}

#[test]
fn resolving_an_unchanged_index_again_is_a_no_op_that_moves_no_generation() {
    // A pass that rewrites rows it already agrees with is a pass that reports work it did not do,
    // and it churns the write-ahead log on every `peek status`. Re-deciding must be free when
    // nothing has changed, which is only true if a decision is compared before it is written.
    let tree = TempTree::new("idempotent");
    tree.write("src/lib.rs", "fn helper() {}\nfn main() { helper(); }\n");

    let mut store = tree.index_without_resolving();
    let first = resolve_all(&mut store, ResolutionOptions::default()).expect("first pass");
    assert!(
        first.committed,
        "the first pass has work to do: {}",
        first.summary()
    );
    let after_first = store.generation();
    assert_eq!(
        first.generation,
        after_first,
        "the report must not invent a generation: {} vs {after_first}",
        first.summary()
    );

    let second = resolve_all(&mut store, ResolutionOptions::default()).expect("second pass");
    assert_eq!(
        second.generation, after_first,
        "a pass that decides nothing must not commit"
    );
    assert!(
        !second.committed,
        "a pass that decides nothing must say so: {}",
        second.summary()
    );
    assert_eq!(second.relations_written, 0, "and must write no rows");
    assert_eq!(
        store.generation(),
        after_first,
        "the store's own counter agrees"
    );
}

#[test]
fn a_definition_that_moves_to_another_file_its_callers_are_re_decided() {
    // Contract G9. `refresh` over the changed paths re-extracts the changed files, but the
    // callers of a moved function live in files that did not change. Without re-reading the
    // relations that *point into* the changed scope, every one of them would keep pointing at a
    // target that is no longer there — a confidently-wrong edge, which is the failure this whole
    // design exists to remove.
    let tree = TempTree::new("moved-definition");
    tree.write("src/one.rs", "pub fn charge() {}\n");
    tree.write("src/driver.rs", "fn go() { charge(); }\n");

    let mut store = tree.index_without_resolving();
    let first = resolve_all(&mut store, ResolutionOptions::default()).expect("first pass");
    assert_eq!(
        first.inferred,
        2,
        "two uniquely named relations to decide — the call and the reference to the same name, \
         because `charge()` both invokes and names it: {}",
        first.summary()
    );
    assert_eq!(
        the_call(&store, "charge").target,
        Some(id("src/one.rs", EntityKind::Function, "charge")),
        "which is where it starts"
    );

    // The definition moves out of `src/one.rs` into `src/two.rs`. `src/driver.rs` is untouched.
    fs::remove_file(tree.path().join("src/one.rs")).expect("remove the original file");
    tree.write("src/one.rs", "pub fn unrelated() {}\n");
    tree.write("src/two.rs", "pub fn charge() {}\n");
    let outcome = refresh(&tree, &mut store, &["src/one.rs", "src/two.rs"]);

    let resolution = outcome
        .report()
        .resolution
        .clone()
        .expect("a refresh resolves");
    assert!(
        resolution.displaced >= 1,
        "the refresh must have seen the edge it was about to break and repaired it: {}",
        resolution.summary()
    );

    let call = the_call(&store, "charge");
    assert_eq!(
        call.target,
        Some(id("src/two.rs", EntityKind::Function, "charge")),
        "the caller in a file that did not change must follow the definition to its new home: {}",
        state_of(&call)
    );
    assert!(
        !store
            .entity(&id("src/one.rs", EntityKind::Function, "charge"))
            .expect("query")
            .is_some(),
        "and the old home is gone from the index, so the edge was re-decided rather than left"
    );
}

#[test]
fn a_scoped_pass_decides_the_paths_it_was_given_and_nothing_else() {
    // A refresh must cost what it touched. If a scoped pass decided the whole index, a one-file
    // edit would be O(repository) again, which is the defect contract G8 exists to catch.
    let tree = TempTree::new("scoped-pass");
    tree.write(
        "src/settled.rs",
        "pub fn already_done() {}\nfn drive() { already_done(); }\n",
    );
    tree.write("src/untouched.rs", "fn go() { not_yet(); }\n");

    let mut store = tree.index_without_resolving();
    let _ = refresh(&tree, &mut store, &["src/settled.rs"]);

    let settled = the_call(&store, "already_done");
    assert_eq!(
        settled.resolution,
        ResolutionState::Resolved {
            by: Evidence::SameFile
        },
        "the file in scope was decided: {}",
        state_of(&settled)
    );

    // The other file was not in scope, so its relation is still pending — which is honest, and
    // a later pass will finish it.
    let untouched = the_call(&store, "not_yet");
    assert!(
        untouched.resolution.is_pending(),
        "a pass scoped to one file must not decide the whole index: {}",
        state_of(&untouched)
    );

    // Scoping to the other file is what finishes it, and it costs only that file.
    let report = resolve_paths(
        &mut store,
        &[RepoPath::new("src/untouched.rs").expect("valid path")],
        &[],
        ResolutionOptions::default(),
    )
    .expect("scope to the second file");
    assert_eq!(
        report.examined,
        2,
        "the pass examined exactly the two relations in the file it was given — the call to \
         `not_yet` and the reference to the same name, because `not_yet()` both invokes and \
         names it, and nothing outside the file: {}",
        report.summary()
    );
    assert_eq!(
        the_call(&store, "not_yet").resolution,
        ResolutionState::Unresolved {
            reason: UnresolvedReason::NoCandidate
        },
        "`not_yet` is named nowhere in the repository, so the only decision available is an \
         unresolved one: {}",
        state_of(&the_call(&store, "not_yet"))
    );
}

/// Run a refresh over `changed`, the way `indexer::refresh` does.
fn refresh(tree: &TempTree, store: &mut Store, changed: &[&str]) -> IndexOutcome {
    let paths: Vec<PathBuf> = changed
        .iter()
        .map(|relative| tree.path().join(relative))
        .collect();
    let outcome = crate::indexer::refresh(store, tree.path(), &paths, &DiscoveryOptions::default())
        .expect("refresh");
    assert!(
        outcome.report().resolution.is_some(),
        "a refresh must run the resolution pass: {}",
        outcome.report().summary()
    );
    outcome
}

// ---------------------------------------------------------------------------
// Failure handling
// ---------------------------------------------------------------------------

#[test]
fn a_failed_resolution_leaves_the_previous_generation_readable() {
    // The property that makes a second pass safe to run at all. A write refused part-way through
    // must roll the whole batch back, the generation must not move, and every decision the
    // previous generation made must still be readable. The store's own suite proves the rollback
    // on a hand-built batch; this proves the *resolver* reaches that path rather than swallowing
    // it, which is the single `.ok()` the engine Peek replaces was built on (audit A1).
    let tree = TempTree::new("failed-pass");
    tree.write("src/lib.rs", "fn helper() {}\nfn main() { helper(); }\n");

    let mut store = tree.index_without_resolving();
    let before = store.generation();
    assert!(before > 0, "the extraction pass committed something");

    let blocker = RefuseWrites::over(&store);
    let failed = resolve_all(&mut store, ResolutionOptions::default());
    blocker.release();

    match failed {
        // A resolver that returned `Ok` here would report a count for work that did not land.
        Err(error) => assert!(
            !matches!(
                error,
                StoreError::Corrupt(_) | StoreError::SchemaTooNew { .. }
            ),
            "a refused write must not be reported as corruption: {error}"
        ),
        Ok(report) => panic!(
            "a refused write must surface as an error, not a report: {}",
            report.summary()
        ),
    }
    assert_eq!(
        store.generation(),
        before,
        "a rolled-back pass is not a commit, or a reader would believe the index is newer"
    );
    store
        .verify()
        .expect("the previous generation is still a valid index");

    // Every relation the rolled-back pass was deciding is untouched: still `Pending`, still
    // carrying the name it was extracted with, and naming no target. Nothing was half-applied.
    let call = the_call(&store, "helper");
    assert!(
        call.resolution.is_pending(),
        "a rolled-back pass must leave the relations it was deciding exactly as it found them: {}",
        state_of(&call)
    );
    assert_eq!(
        call.target,
        None,
        "and must not leave a target behind: {}",
        state_of(&call)
    );
    let still_pending = store
        .relations_in_state(
            &ResolutionState::Pending {
                evidence: Evidence::NameOnly,
                basis: String::new(),
            },
            512,
        )
        .expect("read the pending bucket")
        .len();
    assert!(
        still_pending > 0,
        "the pass had relations to decide, and none of them were decided"
    );

    // And the connection is not poisoned: once the lock is gone the same pass succeeds.
    let recovered = resolve_all(&mut store, ResolutionOptions::default()).expect("retry");
    assert!(
        recovered.committed,
        "a refused pass must not leave the store unable to accept the next one: {}",
        recovered.summary()
    );
    assert_eq!(
        the_call(&store, "helper").resolution,
        ResolutionState::Resolved {
            by: Evidence::SameFile
        },
        "and the retry decides what the rolled-back pass was going to"
    );
}

/// A second connection that holds the database's write lock, so the resolver's commit is refused.
///
/// A read-only filesystem is not portable and replacing the file under an open handle is not
/// portable either, so the fault is injected the way the engine actually fails: another writer
/// got there first. `BEGIN EXCLUSIVE` takes SQLite's write lock and nothing else, which is
/// exactly the condition `Store`'s busy timeout is there to survive.
struct RefuseWrites {
    blocker: rusqlite::Connection,
}

impl RefuseWrites {
    fn over(store: &Store) -> Self {
        let blocker = rusqlite::Connection::open(store.path()).expect("open a second connection");
        blocker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("take the write lock; if this fails the rest of the test proves nothing");
        Self { blocker }
    }

    fn release(self) {
        // Explicit, so the intent is readable rather than a `Drop` impl doing it silently.
        let _ = self.blocker.execute_batch("ROLLBACK");
    }
}

#[test]
fn a_multi_segment_path_resolves_through_the_module_it_names() {
    // R3. `crate::payments::Service::charge` names a module the file can locate, and the
    // extractor recorded that scope precisely so a single-segment path would not have to be
    // guessed. The engine Peek replaces discarded the receiver and the path alike and was left
    // with the bare name `charge` (audit B2, R11).
    let tree = TempTree::new("qualified-scope");
    tree.write(
        "src/payments.rs",
        "pub struct Service;\nimpl Service { pub fn charge(&self) {} }\n",
    );
    tree.write(
        "src/app.rs",
        "fn go() { crate::payments::Service::charge(); }\n",
    );

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let call = the_call(&store, "charge");
    assert_eq!(
        call.target,
        Some(id("src/payments.rs", EntityKind::Method, "Service.charge")),
        "the call must land in the module its path named: {}",
        state_of(&call)
    );
    match &call.resolution {
        ResolutionState::Resolved { by } => assert_eq!(
            by.class(),
            "qualified_name_in_scope",
            "the evidence must name the scope that located it, or explain cannot say why: {}",
            state_of(&call)
        ),
        other => panic!(
            "expected a resolved call, got {other:?}: {}",
            state_of(&call)
        ),
    }
}

#[test]
fn a_fully_qualified_call_into_another_package_is_placed_by_the_module_table() {
    // The rung that resolves `other::module::f()` used to call the path guess directly, so the
    // module table was never asked about the one kind of path it exists to place. The table was
    // reachable from `use` statements and from nowhere else.
    //
    // A workspace is what makes the gap visible, and the reason is structural rather than a
    // matter of anchors tried. The guess joins a module path's segments onto the importer's own
    // directory and each of its ancestors, so from `crates/beta/src/` it builds
    // `crates/beta/src/alpha/gateway.rs`, `crates/beta/alpha/gateway.rs`,
    // `crates/alpha/gateway.rs` and `alpha/gateway.rs` — and never
    // `crates/alpha/src/gateway.rs`, because `src` is not a segment of any module path and
    // nothing in the name mentions it. No anchor list fixes that; the segment is simply absent.
    //
    // The second package is what makes the two arms distinguishable at all. With one `f` in the
    // repository the arm without the table could still reach the right target through the
    // repository-wide rung and the arms would look identical with the table doing nothing. With
    // two, the arm that cannot place the path has to report both and choose neither, which is
    // not a smaller answer but a different one.
    let tree = TempTree::new("qualified-cross-package");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write("crates/alpha/src/gateway.rs", "pub fn f() {}\n");
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "pub fn go() {\n    alpha::gateway::f();\n}\n",
    );
    tree.write("crates/gamma/Cargo.toml", "[package]\nname = \"gamma\"\n");
    tree.write("crates/gamma/src/lib.rs", "pub mod gateway;\n");
    tree.write("crates/gamma/src/gateway.rs", "pub fn f() {}\n");

    let store = tree.index_without_resolving();
    let placed = decide_under(&tree, &store, RelationKind::Calls, true);
    let call = the_one(&placed, "f");

    assert_eq!(
        call.target,
        Some(id("crates/alpha/src/gateway.rs", EntityKind::Function, "f")),
        "the path says `alpha::gateway`, so the call belongs in alpha: {}",
        state_of(call)
    );
    // The rung is asserted as well as the target, because the target alone does not say who
    // produced it. The guess and the table both yield a file, and only the evidence records
    // which one did — so an assertion on the target is an assertion that something placed the
    // call, and this is the assertion that it was this.
    let by = match &call.resolution {
        ResolutionState::Resolved { by } => by,
        other => panic!(
            "expected the call to be resolved by a rung, got {other:?}: {}",
            state_of(call)
        ),
    };
    assert_eq!(
        rung_name(by),
        "scope_qualified_name",
        "the scope rung placed it, and nothing else may: {}",
        state_of(call)
    );

    // The same extraction, decided again with the table off. The guess finds no file, so the
    // rung returns nothing and the call falls through to the rung that searches the whole
    // repository — which cannot tell two packages apart and says so instead of picking.
    let unplaced = decide_under(&tree, &store, RelationKind::Calls, false);
    let call = the_one(&unplaced, "f");
    let candidates = match &call.resolution {
        ResolutionState::Ambiguous { candidates } => candidates.clone(),
        other => panic!(
            "without the table the path cannot be placed at all, and the answer must say so: \
             {other:?}: {}",
            state_of(call)
        ),
    };
    let paths: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.path().as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["crates/alpha/src/gateway.rs", "crates/gamma/src/gateway.rs"],
        "and the two candidates are the two packages the rung cannot tell apart, which is the \
         defect the table exists to remove: {}",
        state_of(call)
    );
}

#[test]
fn a_qualified_call_through_a_type_is_placed_only_by_dropping_the_type_from_the_path() {
    // Pins the `strip_last` argument R3 passes to the module table, from the outside, where a
    // change to it is visible.
    //
    // `alpha::gateway::Service::charge()` puts a **type** in the last position of the scope,
    // because the extractor's `Callee::path` is everything before the final name and a path like
    // this one is a type's associated function, not a module's item. The table has no row for
    // `alpha::gateway::Service` and never will: a type is not a namespace, and no file is named
    // after it. The only reading that hits is the one with that segment dropped, which is the
    // module `alpha::gateway` — the file that declares the type and its `impl` together.
    //
    // So `false` would make this call unplaceable by the table *and* by the guess, and the
    // observable consequence is a state change rather than a wrong target: the call would fall
    // through to the repository-wide rung, land on the same method, and be reported as a claim
    // instead of a proof. A rate over `(resolved + inferred)` cannot see that at all, which is
    // why the measurement counts `Resolved` on its own.
    let tree = TempTree::new("qualified-through-a-type");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/alpha/src/gateway.rs",
        "pub struct Service;\nimpl Service {\n    pub fn charge(&self) {}\n}\n",
    );
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "pub fn go() {\n    alpha::gateway::Service::charge();\n}\n",
    );

    let store = tree.index_without_resolving();
    let placed = decide_under(&tree, &store, RelationKind::Calls, true);
    let call = the_one(&placed, "charge");

    assert_eq!(
        call.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Method,
            "Service.charge"
        )),
        "the path reaches the module that declares the type: {}",
        state_of(call)
    );
    let by = match &call.resolution {
        ResolutionState::Resolved { by } => by,
        other => panic!(
            "expected the call to be resolved by a rung, got {other:?}: {}",
            state_of(call)
        ),
    };
    assert_eq!(
        rung_name(by),
        "scope_qualified_name",
        "and it is a proof, not a claim: the scope rung located the file the type lives in: {}",
        state_of(call)
    );

    // The other arm, on the same extraction. `charge` is unique in the repository, so the rung
    // that follows reaches the same method and reports a *claim* about it. Same target, weaker
    // state, and the rung is what says so.
    let claimed = decide_under(&tree, &store, RelationKind::Calls, false);
    let call = the_one(&claimed, "charge");
    let by = match &call.resolution {
        ResolutionState::Inferred { by, .. } => by,
        other => panic!(
            "without the table this is a claim about a unique name, not a proof: {other:?}: {}",
            state_of(call)
        ),
    };
    assert_eq!(
        rung_name(by),
        "unique_name",
        "the scope rung could not place the path, so the repository-wide one answered: {}",
        state_of(call)
    );
}

#[test]
fn a_capped_candidate_list_is_truncated_and_the_omitted_candidates_are_counted_not_hidden() {
    // The cap exists so a pathological name cannot produce an unbounded row. Hiding the fact that
    // it fired would be worse than the cap: a caller reading `Ambiguous { 32 candidates }` would
    // have no way to know there were 300.
    let tree = TempTree::new("candidate-cap");
    for index in 0..6 {
        tree.write(&format!("src/m{index}.rs"), "pub fn charge() {}\n");
    }
    tree.write("src/driver.rs", "fn go() { charge(); }\n");

    let mut store = tree.index_without_resolving();
    let report = resolve_all(
        &mut store,
        ResolutionOptions::default().with_max_candidates(2),
    )
    .expect("resolve");

    let call = the_call(&store, "charge");
    let candidates = match &call.resolution {
        ResolutionState::Ambiguous { candidates } => candidates.clone(),
        other => panic!("six equally supported candidates must be ambiguous: {other:?}"),
    };
    assert_eq!(
        candidates.len(),
        2,
        "the cap must bound the stored list: {candidates:?}"
    );
    assert!(
        report.truncated > 0,
        "and the pass must report that it dropped candidates rather than presenting two as \
         though they were all: {}",
        report.summary()
    );
    assert!(
        report.summary().contains("truncated"),
        "and the summary must say so: {}",
        report.summary()
    );
}

#[test]
fn a_pass_with_nothing_in_scope_examines_nothing_and_commits_nothing() {
    // A refresh that changed nothing must cost nothing. The test is deliberately blunt: an empty
    // scope, no displaced edges, and re-deciding turned off. Every claim in it is one the
    // implementation could get wrong by doing work anyway, which is exactly the behaviour
    // contract G8 measures.
    let tree = TempTree::new("empty-scope");
    tree.write("src/one.rs", "fn helper() {}\nfn go() { helper(); }\n");

    let mut store = tree.index_without_resolving();
    // The edge is decided first, so a pass that reached outside its scope would have a decided
    // edge to re-examine and this test would fail rather than pass for the wrong reason.
    let first = resolve_all(&mut store, ResolutionOptions::default()).expect("first pass");
    assert!(
        first.examined > 0,
        "the fixture must contain an edge, or an empty scope proves nothing: {}",
        first.summary()
    );
    let before = store.generation();

    let report = resolve_paths(
        &mut store,
        &[],
        &[],
        ResolutionOptions::default().with_reconsider_decided(false),
    )
    .expect("an empty scope is not an error");

    assert_eq!(
        report.examined,
        0,
        "nothing was in scope: {}",
        report.summary()
    );
    assert_eq!(
        report.relations_written,
        0,
        "so nothing was written: {}",
        report.summary()
    );
    assert!(
        !report.committed,
        "and nothing was committed: {}",
        report.summary()
    );
    assert_eq!(
        store.generation(),
        before,
        "an empty pass must not churn the store or move the generation"
    );
    assert!(
        report.summary().contains("no commit"),
        "{}",
        report.summary()
    );
}

#[test]
fn an_ambiguity_widened_by_a_new_file_is_not_re_decided_until_a_full_pass_runs() {
    // **A known limitation, pinned deliberately.**
    //
    // A caller was ambiguous between two `charge`s; a third file appears with a third `charge`.
    // The stale answer is "ambiguous between two" when the truth is three.
    //
    // It cannot be fixed inside a scoped pass with the query surface that exists. An ambiguous
    // relation carries no `target_path`, so [`Store::incoming`] cannot match it; and there is no
    // index from a *target name* back to the relations that name it, so the new file's `charge`
    // cannot be turned into "the edges that mention `charge`". Finding those edges would need
    // either a new index (a schema change) or a full re-resolve (an O(repository) pass on every
    // keystroke), and both are worse than a documented gap.
    //
    // What *does* fix it is asserted below, so the behaviour is a scheduling fact rather than a
    // permanent one: re-extracting the caller's file re-emits its edge as `Pending`, and the pass
    // decides it again with the new file in the index.
    let tree = TempTree::new("widened-ambiguity");
    tree.write("src/one.rs", "pub fn charge() {}\n");
    tree.write("src/two.rs", "pub fn charge() {}\n");
    tree.write("src/driver.rs", "fn go() { charge(); }\n");

    let mut store = tree.index_without_resolving();
    resolve_all(&mut store, ResolutionOptions::default()).expect("first pass");
    let first = the_call(&store, "charge");
    let before = match &first.resolution {
        ResolutionState::Ambiguous { candidates } => candidates.clone(),
        other => panic!("two same-named functions must be ambiguous, got {other:?}"),
    };
    assert_eq!(before.len(), 2, "{before:?}");

    tree.write("src/three.rs", "pub fn charge() {}\n");
    let _ = refresh(&tree, &mut store, &["src/three.rs"]);

    let after = the_call(&store, "charge");
    let stale = match &after.resolution {
        ResolutionState::Ambiguous { candidates } => candidates.clone(),
        other => panic!("still ambiguous, got {other:?}: {}", state_of(&after)),
    };
    assert_eq!(
        stale, before,
        "a scoped pass cannot widen an ambiguity it cannot locate, and must not pretend to"
    );

    // Re-extracting the *caller's* file is what corrects it: the extractor re-emits the edge as
    // `Pending` with the same target name, and the pass decides it with all three definitions in
    // the index. The scope is still one file, so this is cheap — the caller had to be touched for
    // the engine to know its answer had gone stale.
    tree.write("src/driver.rs", "fn go() { charge(); }\n");
    let _ = refresh(&tree, &mut store, &["src/driver.rs"]);

    let repaired = the_call(&store, "charge");
    let widened = match &repaired.resolution {
        ResolutionState::Ambiguous { candidates } => candidates.clone(),
        other => panic!("still ambiguous, got {other:?}: {}", state_of(&repaired)),
    };
    assert_eq!(
        widened,
        vec![
            id("src/one.rs", EntityKind::Function, "charge"),
            id("src/three.rs", EntityKind::Function, "charge"),
            id("src/two.rs", EntityKind::Function, "charge"),
        ],
        "re-deciding the caller's own edge must find the third definition, in a stable order"
    );
}

/// A file with far more entities in it than a small page can hold, and one call written in the
/// **last** of them by identity order.
///
/// Two things about the fixture are deliberate, and both are there to make a partial fix fail:
///
/// * The caller's qualified name sorts last (`zzz_drive` after `f19`), because
///   `Store::entities_in_file_after` orders by `(kind, qualified_name, entity_ordinal)` — so a
///   reader that stops after a page stops before the caller, and a reader that stops before the
///   caller never learns the call exists.
/// * The callee is in the tail as well, so the *ladder's own* read of the source file is exercised
///   separately from the scope enumeration. A pass that enumerated the whole file but then decided
///   each relation against a bounded prefix of it would find no `f19` in the same file, fall
///   through to the repository-wide rung, and store `inferred (unique_name)` — a different, weaker
///   answer, produced by the same defect. Asserting the rung rather than only the state catches it;
///   asserting `pending_remaining` alone would not.
fn wide_file(label: &str) -> TempTree {
    let tree = TempTree::new(label);
    let mut source = String::new();
    for index in 0..20 {
        source.push_str(&format!("fn f{index:02}() {{}}\n"));
    }
    source.push_str("fn zzz_drive() { f19(); }\n");
    tree.write("src/lib.rs", &source);
    tree
}

/// Every relation in the index, described, in a form two builds can be compared by.
///
/// The five states partition the relation set, so enumerating each one enumerates the index — and
/// comparing the whole list rather than a count is what lets a comparison notice *which* relation
/// changed. A test that compared counts could pass on two indexes that differ in every answer and
/// happen to balance.
fn every_decision(store: &Store) -> Vec<String> {
    let mut rows = Vec::new();
    // `relations_in_state` reads the tag, so only the variants matter; the payloads are the
    // resolver's own placeholders, kept identical to the ones the pass asks for.
    for state in [
        ResolutionState::Pending {
            evidence: Evidence::NameOnly,
            basis: String::new(),
        },
        ResolutionState::Resolved {
            by: Evidence::NameOnly,
        },
        ResolutionState::Inferred {
            by: Evidence::NameOnly,
            basis: String::new(),
        },
        ResolutionState::Ambiguous {
            candidates: Vec::new(),
        },
        ResolutionState::Unresolved {
            reason: UnresolvedReason::NoCandidate,
        },
    ] {
        for relation in store
            .relations_in_state(&state, 4096)
            .expect("read one resolution state")
        {
            rows.push(format!(
                "{:?} {} -> {} [{}] {:?}",
                relation.kind,
                relation.source,
                relation.target_name,
                relation.resolution.describe(),
                relation.target,
            ));
        }
    }
    rows.sort();
    rows
}

#[test]
fn a_file_larger_than_the_page_is_read_to_the_end_and_leaves_nothing_pending() {
    // The defect, as it was measured. On `BurntSushi/ripgrep`, `crates/core/flags/defs.rs` holds
    // 1,363 entities; a refresh of it decided 2,121 of the file's 3,599 relations and left **348**
    // `Pending` — extracted, never placed, never refused. `2,121 + 348 = 2,469` is the whole of
    // what that pass was responsible for, and the lowest-ranked pending source was #514, one row
    // past the bound of 512. Both halves matter: the sum says the missing work was the part outside
    // the pass rather than a class of relation the ladder declined, and the rank says *why* — a
    // declined class would not line up with the position of the bound.
    //
    // A second refresh of that file changed nothing (348 → 348), because the bound cut the same
    // tail off again, and only `resolve_all` clears them — which `build_full` alone calls. So the
    // leak grew one large file at a time on the operation `watch` runs on every keystroke.
    //
    // This is that measurement as a test, at a size a test can afford. A page of 4 rows against a
    // file of twenty-plus entities is the same arithmetic.
    let tree = wide_file("paged-file");
    let mut store = tree.index_without_resolving();

    let report = resolve_paths(
        &mut store,
        &[RepoPath::new("src/lib.rs").expect("valid path")],
        &[],
        ResolutionOptions::default().with_entities_page(4),
    )
    .expect("a scoped pass over a file larger than its page");

    assert!(
        !report.pending_remaining,
        "a file larger than the page must be read to the end: a bounded read left the tail of every \
         large file `Pending`, and nothing afterwards revisited it: {}",
        report.summary()
    );
    assert_eq!(
        report.truncated,
        0,
        "and a pass that read the whole file reports no truncation. The bound is a page size now, \
         not a cut-off, so a file being large is not something the pass failed to do: {}",
        report.summary()
    );

    // The rung, not only the state. `f19` is in the tail of the source file too, so a pass that
    // enumerated the file in full but decided against a bounded prefix of it would report the call
    // `inferred (unique_name)` — still a decision, still no pending relation, and still wrong.
    let call = the_call(&store, "f19");
    assert_eq!(
        call.resolution,
        ResolutionState::Resolved {
            by: Evidence::SameFile
        },
        "the call's target is declared in the same file, and that file is larger than the page, so \
         the same-file rung is the only one that can honestly answer: {}",
        state_of(&call)
    );
    assert_eq!(
        call.target,
        Some(id("src/lib.rs", EntityKind::Function, "f19")),
        "and it points at the definition the fixture wrote: {}",
        state_of(&call)
    );
    assert!(
        report.examined > 0,
        "the pass must have looked at something, or the assertions above pass for the wrong reason: \
         {}",
        report.summary()
    );
    assert_eq!(
        report.resolved + report.inferred + report.ambiguous + report.unresolved,
        report.examined,
        "and every relation it looked at must be accounted for: {}",
        report.summary()
    );
}

#[test]
fn the_page_size_is_a_page_size_and_does_not_change_the_answer() {
    // The contract the option now carries, stated as an equality rather than as prose: the bound
    // decides **how many reads** the pass makes and nothing about **what it decides**.
    //
    // Two arms off two independently built copies of byte-identical input. Not two passes over one
    // index — a second pass over the same index would find nothing pending and compare an empty
    // answer against a full one, which is how a comparison that proves nothing gets written. And
    // not two builds of different trees: the whole point is that only the page size differs.
    let narrow = wide_file("page-narrow");
    let roomy = wide_file("page-roomy");

    let mut narrow_store = narrow.index_without_resolving();
    let narrow_report = resolve_all(
        &mut narrow_store,
        ResolutionOptions::default().with_entities_page(1),
    )
    .expect("resolve one entity at a time");
    let mut roomy_store = roomy.index_without_resolving();
    let roomy_report = resolve_all(
        &mut roomy_store,
        ResolutionOptions::default().with_entities_page(4096),
    )
    .expect("resolve in one page");

    assert_eq!(
        narrow_report.truncated,
        0,
        "a page size of one row reads the file twenty times over and truncates nothing: {}",
        narrow_report.summary()
    );
    assert_eq!(
        roomy_report.truncated,
        0,
        "and a page size above the file's entity count truncates nothing either: {}",
        roomy_report.summary()
    );
    assert_eq!(
        every_decision(&narrow_store),
        every_decision(&roomy_store),
        "one page and one row per page must reach the same decisions; the bound is a page size, so \
         it may not move an answer"
    );
}

#[test]
fn a_page_size_of_zero_reads_the_file_rather_than_nothing() {
    // A page that can hold no rows comes back short, and a short page says "that was the last
    // page". So a page size of zero would report every file in the repository as empty, resolve
    // every relation to `no_candidate`, and print a report saying nothing was wrong. It is one
    // clamp and a whole class of silent wrongness, so it is pinned rather than left to a reader to
    // infer.
    let tree = wide_file("page-zero");
    let mut store = tree.index_without_resolving();

    let report = resolve_all(
        &mut store,
        ResolutionOptions::default().with_entities_page(0),
    )
    .expect("a page size of zero is a page size");

    let call = the_call(&store, "f19");
    assert_eq!(
        call.resolution,
        ResolutionState::Resolved {
            by: Evidence::SameFile
        },
        "the file was read, so the call resolves the way it does at any other page size: {}",
        state_of(&call)
    );
    assert!(
        !report.pending_remaining,
        "and nothing was left undecided: {}",
        report.summary()
    );
}

#[test]
fn a_truncated_candidate_lookup_is_reported_rather_than_silently_accepted() {
    // A limit that is not visible is a limit that quietly changes the answer.
    //
    // **This fixture changed, and the old one asserted a truth that is now false by design.** It
    // used to shrink `entities_per_file` below the file's entity count and require the pass to
    // report a truncation — which it did, and which is exactly what the pass no longer does: a
    // file's entity list is paged, so a file bigger than the page is a page *count* and not a
    // truncation. Reading that assertion as still-required would have pinned the defect back in.
    // The lookup this is now about is the one that can still cut: a name's candidate set.
    //
    // Three files declare `charge` and a fourth calls it, so the name lookup finds three
    // candidates. A bound of one row stops after the first, so the rung has exactly one candidate
    // and would answer "exactly one entity named `charge` is indexed" — a sentence about the whole
    // repository, derived from one row, stored in the field whose purpose is to be auditable. So
    // the pass refuses instead, and says why in the count.
    let tree = TempTree::new("truncation-reported");
    tree.write("src/one.rs", "pub fn charge() {}\n");
    tree.write("src/two.rs", "pub fn charge() {}\n");
    tree.write("src/three.rs", "pub fn charge() {}\n");
    tree.write("src/driver.rs", "fn go() { charge(); }\n");

    let mut store = tree.index_without_resolving();
    let report = resolve_all(
        &mut store,
        ResolutionOptions::default().with_entities_by_name(1),
    )
    .expect("resolve");

    assert!(
        report.truncated > 0,
        "a name lookup that stopped at its bound must be recorded: {}",
        report.summary()
    );
    assert!(
        report.summary().contains("truncated"),
        "and the summary must say so: {}",
        report.summary()
    );

    // The refusal, and the discriminating half: one row was read and three declarations exist, so
    // the uniqueness claim is false and the relation must come out unestablished rather than
    // confidently pointed at whichever file sorted first.
    let call = the_call(&store, "charge");
    assert_eq!(
        call.resolution,
        ResolutionState::Unresolved {
            reason: UnresolvedReason::NoCandidate
        },
        "a uniqueness check read from a window that stopped at the bound is not a uniqueness check, \
         so the rung declines and the edge is visibly unestablished instead of wrong: {}",
        state_of(&call)
    );
}

#[test]
fn the_uniqueness_refusal_is_a_consequence_of_the_bound_and_not_of_the_rung() {
    // The discriminator for the refusal above. Same fixture, same three declarations, same call —
    // only the bound on the name lookup differs, and the answer must differ with it.
    //
    // Without this second arm the refusal could be satisfied by deleting R5 outright, which would
    // turn a class of correct `inferred` answers into `no_candidate` and still pass the test above.
    // What is being pinned is that the *bound* decides, not the rung.
    let roomy = TempTree::new("uniqueness-roomy");
    roomy.write("src/one.rs", "pub fn charge() {}\n");
    roomy.write("src/two.rs", "pub fn charge() {}\n");
    roomy.write("src/three.rs", "pub fn charge() {}\n");
    roomy.write("src/driver.rs", "fn go() { charge(); }\n");

    let mut store = roomy.index_without_resolving();
    let report = resolve_all(
        &mut store,
        ResolutionOptions::default().with_entities_by_name(64),
    )
    .expect("resolve");

    assert_eq!(
        report.truncated,
        0,
        "a bound above the candidate count cuts nothing, so there is nothing to refuse: {}",
        report.summary()
    );
    let call = the_call(&store, "charge");
    let candidates = match &call.resolution {
        ResolutionState::Ambiguous { candidates } => candidates.clone(),
        other => panic!(
            "three equally supported declarations are an ambiguity, not a refusal — with nothing \
             cut short the rung must answer: {other:?}: {}",
            state_of(&call)
        ),
    };
    assert_eq!(
        candidates.len(),
        3,
        "and every one of them is written down, because a cut-short list is not the case here: \
         {candidates:?}"
    );
}

#[test]
fn a_cut_short_edge_read_is_reported_and_still_leaves_the_pass_incomplete() {
    // **The cut-offs that remain, pinned so they cannot go quietly.** Paging closed the one bound
    // that cut a pass short without the caller asking for it. Three bounds still cut because they
    // bound a *search* rather than an enumeration: a name's candidate set, and the two adjacency
    // reads. This is the adjacency half.
    //
    // One source with two outgoing calls, and a bound of one row per source. The pass therefore
    // reads one of the two and never learns the other exists, which is the same shape as the defect
    // this work started from — so the assertion that matters is the **count**, not the state. Before
    // this change `resolve_paths` abandoned those reads without recording anything, so a caller saw
    // a pass that reported success and left an edge undecided, which is the worst of both.
    //
    // The pass is still incomplete here, and that is stated rather than hidden: a caller who asks
    // for a bound of one row per source gets a pass over one row per source. What it is entitled
    // to is the number, and it now gets it. The difference from the entity bound is the whole
    // point — `entities_page` was a bound the caller never chose and a default that silently
    // truncated, whereas `outgoing_per_source` is a bound the caller set and can see.
    let tree = TempTree::new("edge-bound-reported");
    tree.write("src/driver.rs", "fn go() { alpha(); beta(); }\n");

    let mut store = tree.index_without_resolving();
    let report = resolve_paths(
        &mut store,
        &[RepoPath::new("src/driver.rs").expect("valid path")],
        &[],
        ResolutionOptions {
            outgoing_per_source: 1,
            ..ResolutionOptions::default()
        },
    )
    .expect("a pass whose edge bound cuts short");

    assert!(
        report.truncated > 0,
        "an edge read that stopped at its bound has to be counted: {}",
        report.summary()
    );
    assert!(
        report.summary().contains("truncated"),
        "and the summary has to say so: {}",
        report.summary()
    );

    let pending: Vec<String> = relations_of(&store, RelationKind::Calls)
        .into_iter()
        .filter(|relation| relation.resolution.is_pending())
        .map(|relation| relation.target_name.clone())
        .collect();
    assert_eq!(
        pending.len(),
        1,
        "one row per source leaves exactly one of the two calls outside the pass, and the count \
         above is what tells the caller that: {pending:?}"
    );
    assert!(
        report.pending_remaining,
        "and the pass says it left work behind, which is the difference between a reported limit \
         and a silent one: {}",
        report.summary()
    );
}

#[test]
fn every_relation_the_pass_examined_is_accounted_for_and_none_is_left_pending() {
    // The report and the store are two descriptions of one pass. If they disagree, one of them is
    // fiction, and a `peek status` that disagrees with the index is worse than no status at all.
    // The four decision counts must account for every examined relation: a relation that is
    // neither decided nor counted is one the resolver dropped, which is the defect this exists
    // to remove.
    let tree = TempTree::new("report-matches-store");
    tree.write("src/one.rs", "pub fn charge() {}\n");
    tree.write("src/two.rs", "pub fn charge() {}\n");
    tree.write("src/three.rs", "pub fn charge() {}\n");
    tree.write("src/driver.rs", "fn go() { charge(); missing_one(); }\n");
    tree.write(
        "src/inherits.rs",
        "trait Base {}\ntrait Extended: Base {}\n",
    );

    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    assert_eq!(
        report.resolved + report.inferred + report.ambiguous + report.unresolved,
        report.examined,
        "every examined relation must land in exactly one decision bucket: {}",
        report.summary()
    );
    assert!(
        report.ambiguous >= 1,
        "the fixture has an ambiguous call: {}",
        report.summary()
    );
    assert!(
        report.unresolved >= 1,
        "and an unknown one: {}",
        report.summary()
    );
    assert!(
        !report.pending_remaining,
        "a pass must leave nothing pending behind it: {}",
        report.summary()
    );

    // The store agrees, state by state. The extractor never writes an `Ambiguous`, so this
    // bucket is entirely the resolver's and the comparison is exact.
    let ambiguous = store
        .relations_in_state(
            &ResolutionState::Ambiguous {
                candidates: Vec::new(),
            },
            512,
        )
        .expect("read the ambiguous bucket")
        .len() as u64;
    assert_eq!(
        report.ambiguous,
        ambiguous,
        "the report's ambiguous count must be the store's: {}",
        report.summary()
    );

    let pending = store
        .relations_in_state(
            &ResolutionState::Pending {
                evidence: Evidence::NameOnly,
                basis: String::new(),
            },
            512,
        )
        .expect("read the pending bucket")
        .len() as u64;
    assert_eq!(pending, 0, "and nothing is still awaiting a decision");
}

// ---------------------------------------------------------------------------
// Which route turns a module path into a file
// ---------------------------------------------------------------------------

/// A crate root with enough declarations in it that a kind-ordered prefix cannot reach a
/// `Module` row.
///
/// This is the shape that made `package_of` answer `None` on a real repository: the read it used
/// was `ORDER BY kind, qualified_name, entity_ordinal LIMIT n`, and `Module` and `Package` sort
/// *after* `function`, so on any file with more than `n` declarations the namespace row was not in
/// the page. Ten functions is comfortably more than the bound of eight.
fn crowded_crate_root(imports: &str) -> String {
    let mut source = String::from(imports);
    for index in 0..10 {
        source.push_str(&format!("pub fn helper_{index}() {{}}\n"));
    }
    source
}

#[test]
fn a_crowded_crate_root_still_names_its_own_package() {
    // The importer's package is what turns `crate::a::Item` into `pkg::a::Item`, and without it the
    // table can only offer the unprefixed `a` — which is the `mod a;` **declaration** row in this
    // very file, not the module's own file. So the answer comes back as the file that declares the
    // module, the declaration is not in it, the rung declines, and the edge falls to the one rung
    // that can only claim. That is the shape of every intra-crate proof the recorded measurement
    // says the module table was costing.
    let tree = TempTree::new("crowded-crate-root");
    tree.write("src/a.rs", "pub struct Item;\n");
    tree.write(
        "src/lib.rs",
        &crowded_crate_root("pub mod a;\nuse crate::a::Item;\npub fn go() -> Item { Item }\n"),
    );
    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let item = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "Item")
        .expect("the import of Item was extracted");
    assert_eq!(
        item.target,
        Some(id("src/a.rs", EntityKind::Struct, "Item")),
        "the import names the file the module *is*, not the file that declares it: {}",
        state_of(&item)
    );
    assert_eq!(
        rung_name(&decided_by(&item)),
        "import_binding",
        "and the rung is still the author's own import: {}",
        state_of(&item)
    );
    // The counter, not the placement: this path is crate-relative, so the guess answers it even
    // when the package is unknown and the placement above would pass either way. What must not
    // regress is the package lookup, because a path that names *another* package is answered by
    // the table and it has nothing to offer without one.
    assert_eq!(
        report.module_files.package_unknown, 0,
        "every lookup named the importer's package, which is what the crowded crate root used to \
         prevent: {:?}",
        report.module_files
    );
}

#[test]
fn a_crowded_crate_root_still_names_its_own_package_for_the_table() {
    // The same crowded crate root, read through the route that needs the package. A path naming
    // another package is answered by the table, and the table builds every spelling it searches
    // from the importing file's package; with the package unknown it falls through to the bare
    // spelling, and the bare spelling of `alpha::gateway::Gateway` is a row this index holds for
    // something else.
    let tree = TempTree::new("crowded-crate-root-cross");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write("crates/alpha/src/gateway.rs", "pub struct Gateway;\n");
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        &crowded_crate_root("use alpha::gateway::Gateway;\npub fn go() -> Gateway { Gateway }\n"),
    );
    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let gateway = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "Gateway")
        .expect("the import of Gateway was extracted");
    assert_eq!(
        gateway.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Struct,
            "Gateway"
        )),
        "ten declarations in the importing file did not stop the table naming the package the \
         import means: {}",
        state_of(&gateway)
    );
    assert_eq!(
        report.module_files.package_unknown, 0,
        "and the report says the prefix was available for every lookup: {:?}",
        report.module_files
    );
}

#[test]
fn a_declaration_row_does_not_answer_a_path_about_the_module() {
    // Both spellings of `alpha::gateway::Gateway` name a file in this index: `beta::alpha::gateway`
    // is `crates/beta/src/alpha/gateway.rs` and `alpha::gateway` is
    // `crates/alpha/src/gateway.rs`. The path names the package `alpha`, so the bare spelling is
    // the one that means it and the prefixed one is a coincidence — and collecting the union of
    // them hands the rung two candidate files, which is an `Ambiguous` rather than an answer.
    //
    // `Gateway` is declared in both files, so the union is a real ambiguity rather than a lucky
    // single hit, and the edge belongs in `alpha` because the import says so.
    let tree = TempTree::new("first-spelling-wins");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/alpha/src/gateway.rs",
        "pub struct Gateway;\npub fn here() {}\n",
    );
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write("crates/beta/src/lib.rs", "pub mod alpha;\n");
    tree.write(
        "crates/beta/src/alpha/gateway.rs",
        "pub struct Gateway;\npub fn here() {}\n",
    );
    tree.write(
        "crates/beta/src/app.rs",
        "use alpha::gateway::Gateway;\npub fn go() -> Gateway { Gateway }\n",
    );
    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let gateway = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "Gateway")
        .expect("the import of Gateway was extracted");
    assert_eq!(
        gateway.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Struct,
            "Gateway"
        )),
        "the spelling that means the package the path named answered, and the importing crate's \
         own module of the same name did not: {}",
        state_of(&gateway)
    );
    assert!(
        report.module_files.table_answered_named > 0,
        "and the table is what answered a path that named a package: {:?}",
        report.module_files
    );
}

#[test]
fn the_guess_answers_a_path_the_table_cannot_spell() {
    // A crate that keeps its modules directly under the crate directory, with no `src`. The
    // extractor's package rule takes the directory above the first source root, and there is none,
    // so every file is named for its own parent: `core/flags/defs.rs` becomes the module
    // `flags::defs` and its package is `flags`. A `crate::flags::Flag` path cannot be spelled from
    // there — and the bare `flags` that the table falls through to is the `mod flags;` row in
    // `core/main.rs`, which declares no `Flag`.
    //
    // That half is `extract::modules::locate` and is not this file's to fix. The half that is this
    // file's is that the guess is anchored to the *referring file*, so it finds
    // `core/flags/mod.rs` — a file inside the referring file's own directory chain, which is what
    // an import written in it means — and the table's answer is not.
    let tree = TempTree::new("unguessed-package");
    tree.write("core/flags/mod.rs", "pub struct Flag;\n");
    tree.write("core/main.rs", "mod flags;\nfn main() {}\n");
    tree.write(
        "core/flags/defs.rs",
        "use crate::flags::Flag;\npub fn go() -> Flag { Flag }\n",
    );
    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let flag = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "Flag")
        .expect("the import of Flag was extracted");
    assert_eq!(
        flag.target,
        Some(id("core/flags/mod.rs", EntityKind::Struct, "Flag")),
        "the file the referring file's own directory chain names is where the declaration is: {}",
        state_of(&flag)
    );
    assert!(
        report.module_files.guess_answered > 0,
        "and the report says the guess is what answered: {:?}",
        report.module_files
    );
}

#[test]
fn the_table_answers_a_package_the_guess_cannot_reach() {
    // The other half, and it is the half the module table exists for. `src` is not a segment of a
    // module path, so no anchor list can produce `crates/alpha/src/gateway.rs` from
    // `crates/beta/src/`: the guess is structurally incapable of it and this is not a preference
    // between two heuristics.
    let tree = TempTree::new("table-reaches-a-package");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write("crates/alpha/src/gateway.rs", "pub struct Gateway;\n");
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "use alpha::gateway::Gateway;\npub fn go() -> Gateway { Gateway }\n",
    );
    // A same-named module inside the importer's own package, so the guess has something to be wrong
    // about: `src/gateway.rs` exists and declares a `Gateway` too.
    tree.write("crates/beta/src/gateway.rs", "pub struct Gateway;\n");

    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");

    let gateway = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "Gateway")
        .expect("the import of Gateway was extracted");
    assert_eq!(
        gateway.target,
        Some(id(
            "crates/alpha/src/gateway.rs",
            EntityKind::Struct,
            "Gateway"
        )),
        "the path names another package, so the edge belongs there and not beside the import: {}",
        state_of(&gateway)
    );
    assert!(
        report.module_files.table_answered > 0,
        "and the table is what answered it, because the guess could not: {:?}",
        report.module_files
    );
}

#[test]
fn the_module_file_counters_partition_the_lookups() {
    // A report whose counters do not add up is a report that cannot be read, so this is the check
    // that keeps them honest: three outcomes and no fourth, and every lookup in exactly one of them.
    let tree = TempTree::new("counters-partition");
    tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
    tree.write("crates/alpha/src/lib.rs", "pub mod gateway;\n");
    tree.write("crates/alpha/src/gateway.rs", "pub struct Gateway;\n");
    tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\n");
    tree.write(
        "crates/beta/src/lib.rs",
        "use alpha::gateway::Gateway;\nuse crate::nothing::Here;\npub fn go() { let _ = Gateway; }\n",
    );
    let mut store = tree.index_without_resolving();
    let report = resolve_all(&mut store, ResolutionOptions::default()).expect("resolve");
    let files = report.module_files;

    assert_eq!(
        files.lookups,
        files.table_answered + files.guess_answered + files.neither,
        "the three outcomes must partition the lookups: {files:?}"
    );
    assert_eq!(
        files.table_answered,
        files.table_answered_named + files.table_answered_crate_relative,
        "every table answer was a package path or a crate-relative one: {files:?}"
    );
    assert_eq!(
        files.package_unknown, 0,
        "and both crates are spelled by their own `Cargo.toml`, so every importer's package was \
         named: {files:?}"
    );
    assert!(
        files.lookups > 0 && files.asked > 0,
        "the pass asked the table something, or the counters are counting nothing: {files:?}"
    );
    assert!(
        report.summary().contains("module files:"),
        "and the summary carries them, because a number nobody can read is a number nobody reads: \
         {}",
        report.summary()
    );
}
