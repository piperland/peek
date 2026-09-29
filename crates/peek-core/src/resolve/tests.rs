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
use crate::model::relation::{Evidence, RelationKind, ResolutionState, UnresolvedReason};
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

    let mut store = tree.index_without_resolving();
    let options = ResolutionOptions {
        use_module_table: true,
        ..ResolutionOptions::default()
    };
    resolve_all(&mut store, options).expect("resolve with the module table on");

    let import = relations_of(&store, RelationKind::Imports)
        .into_iter()
        .find(|relation| relation.target_name == "Gateway")
        .expect("the import of Gateway was extracted");
    assert_eq!(
        state_of(&import),
        "ambiguous (2 candidates)",
        "the module table DID place it — the candidate is `crates/alpha/src/gateway.rs`, a path \
         the guess cannot construct — but the answer is ambiguous, and that is a defect: the two \
         candidates are the `struct Gateway` and a *second* entity also named `Gateway` of kind \
         `Module`, which is the `impl Gateway` block. An impl block is emitted as an entity \
         because the walker's scope stack needs an id to anchor methods to, and that entity then \
         competes for the type's own name in every lookup. Recorded as R-012: an impl block is a \
         scope, not a declaration, and it must not be findable as one. The relation: {import:?}"
    );

    let call = relations_of(&store, RelationKind::Calls)
        .into_iter()
        .find(|relation| relation.target_name == "send")
        .expect("the call to send was extracted");
    assert_eq!(
        state_of(&call),
        "resolved",
        "and the method call inside the other package is placed outright: {call:?}"
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
    // With one `Gateway` in the repository, both arms report the same ambiguity and the switch
    // could be broken with nothing to show it. The candidate *count* is the discriminator: the
    // module table can only see the file it looked up, so it finds alpha's two entities (the
    // struct and the phantom impl) and stops. The repository-wide rungs have no package boundary
    // to respect, so they find every `Gateway` in the tree. Different counts, same state, and the
    // difference is exactly the capability being measured.
    tree.write("crates/gamma/Cargo.toml", "[package]\nname = \"gamma\"\n");
    tree.write("crates/gamma/src/lib.rs", "pub mod gateway;\n");
    tree.write(
        "crates/gamma/src/gateway.rs",
        "pub struct Gateway;\n",
    );

    let mut store = tree.index_without_resolving();

    let candidates = |options: ResolutionOptions| -> Vec<RepoPath> {
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
        match import.resolution {
            ResolutionState::Ambiguous { candidates } => {
                candidates.iter().map(|id| id.path().clone()).collect()
            }
            other => panic!("expected an ambiguity to compare, got {other:?}"),
        }
    };

    let with_table = candidates(ResolutionOptions {
        use_module_table: true,
        ..ResolutionOptions::default()
    });
    let without_table = candidates(ResolutionOptions {
        use_module_table: false,
        ..ResolutionOptions::default()
    });

    assert_eq!(
        with_table.len(),
        2,
        "the table sees only alpha's file: the struct and the phantom impl: {with_table:?}"
    );
    assert!(
        with_table.iter().all(|path| path.as_str() == "crates/alpha/src/gateway.rs"),
        "every candidate is in the package the import named: {with_table:?}"
    );
    assert!(
        without_table.len() > with_table.len(),
        "without the table the repository-wide rungs see gamma's Gateway too, which is the whole \
         problem: the fallback cannot tell which package an import meant: {without_table:?}"
    );
    assert!(
        without_table
            .iter()
            .any(|path| path.as_str() == "crates/gamma/src/gateway.rs"),
        "and gamma is exactly the wrong answer: {without_table:?}"
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
        1,
        "one uniquely named call to decide: {}",
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
        1,
        "the pass examined exactly the one relation in the file it was given: {}",
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

#[test]
fn a_truncated_candidate_lookup_is_reported_rather_than_silently_accepted() {
    // A limit that is not visible is a limit that quietly changes the answer. A file with more
    // entities than the pass will read must be recorded as truncated, so a caller can tell an
    // honest "no candidate" from an incomplete search.
    let tree = TempTree::new("truncation-reported");
    let mut source = String::from("fn main() { helper(); }\n");
    for index in 0..12 {
        source.push_str(&format!("fn helper_{index}() {{}}\n"));
    }
    tree.write("src/lib.rs", &source);

    let mut store = tree.index_without_resolving();
    let report = resolve_all(
        &mut store,
        ResolutionOptions::default()
            .with_entities_per_file(4)
            .with_entities_by_name(4),
    )
    .expect("resolve");

    assert!(
        report.truncated > 0,
        "a limit smaller than the file must be recorded: {}",
        report.summary()
    );
    assert!(
        report.summary().contains("truncated"),
        "and the summary must say so: {}",
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
