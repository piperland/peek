//! Measuring one language against its labelled fixture.
//!
//! Every dimension here answers a question with a numerator and a denominator,
//! and every denominator is a population somebody wrote down. Nothing is a
//! pass/fail boolean on its own: the gate reports counts per resolution class and
//! the numbers are the finding.
//!
//! # How the graph is read
//!
//! Through the public API only — `build_full`, `refresh`, the store's own
//! queries, and `Query`. If the gate needed a private accessor it would be
//! measuring something the engine does not actually expose, and the number would
//! be about the gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use peek_core::discover::DiscoveryOptions;
use peek_core::indexer::{self, IndexReport};
use peek_core::model::{
    EntityId, Evidence, Language, Relation, RelationKind, ResolutionState, UnresolvedReason,
};
use peek_core::query::{Direction, Query, QueryError, WalkRequest};
use peek_core::store::Store;

use super::expect::{Corpus, Key, Labelled, LabelledCall, LabelledPair};
use super::score::{Fraction, Match, Multiset};

/// Every dimension E4 names, in the order the matrix prints them.
///
/// The floor file, the matrix and the pass/fail check all iterate this list, so a
/// dimension cannot be measured and then left out of the published table.
pub const DIMENSIONS: &[&str] = &[
    "symbol_precision",
    "symbol_recall",
    "definitions",
    "calls",
    "references",
    "imports",
    "members",
    "inheritance_subject",
    "inheritance_base",
    "negative_references",
    "negative_inheritance",
    "incremental",
    "query",
    "context",
];

/// One dimension's number.
#[derive(Debug, Clone, Copy)]
pub struct Dimension {
    pub name: &'static str,
    pub value: Fraction,
}

/// Everything the matrix prints for one language.
#[derive(Debug, Clone)]
pub struct Measurement {
    pub language: Language,
    pub dimensions: Vec<Dimension>,
    /// Entity counts by kind, including the structural kinds held out of the
    /// symbol denominator. Published so the exclusion is visible.
    pub entity_counts: BTreeMap<String, u64>,
    /// Relation counts by class, then by resolution state.
    pub class_states: BTreeMap<String, BTreeMap<String, u64>>,
    /// Labels the engine did not produce, per dimension. The evidence for the
    /// number rather than a decoration on it.
    pub missing: BTreeMap<&'static str, Vec<String>>,
    /// Relations the engine produced that no label accounts for, per class.
    pub spurious: BTreeMap<&'static str, Vec<String>>,
    /// The query assertions that failed, in full.
    pub query_failures: Vec<String>,
    /// The context assertions that failed, in full.
    pub context_failures: Vec<String>,
    /// What a full build reported, verbatim.
    pub index_report: IndexReport,
    /// Entities held out of the symbol denominator, and why.
    pub structural_entities: u64,
}

impl Measurement {
    pub fn get(&self, name: &str) -> Fraction {
        self.dimensions
            .iter()
            .find(|dimension| dimension.name == name)
            .map_or_else(|| Fraction::new(0, 0), |dimension| dimension.value)
    }
}

// ---------------------------------------------------------------------------
// The graph, read out of the store
// ---------------------------------------------------------------------------

/// One entity row, keyed the way `EntityId` is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntityRow {
    pub path: String,
    pub kind: String,
    pub qualified_name: String,
    pub ordinal: u32,
}

impl EntityRow {
    pub fn key(&self) -> Key {
        Key::new(&self.path, &self.kind, &self.qualified_name)
    }

    pub fn render(&self) -> String {
        format!(
            "{} | {} | {} | #{}",
            self.path, self.kind, self.qualified_name, self.ordinal
        )
    }
}

fn row_of(id: &EntityId) -> EntityRow {
    EntityRow {
        path: id.path().as_str().to_owned(),
        kind: id.kind().as_str().to_owned(),
        qualified_name: id.qualified_name().to_owned(),
        ordinal: id.ordinal(),
    }
}

fn render_id(id: &EntityId) -> String {
    format!(
        "{} | {} | {}",
        id.path().as_str(),
        id.kind().as_str(),
        id.qualified_name()
    )
}

/// One relation row, flattened for counting.
#[derive(Debug, Clone)]
pub struct RelationRow {
    pub kind: &'static str,
    pub state: &'static str,
    pub evidence: &'static str,
    pub source: EntityRow,
    pub target_name: String,
    pub target: Option<EntityRow>,
    pub module: Option<String>,
    pub alias: Option<String>,
}

impl RelationRow {
    /// A stable identity for counting, independent of row ordering.
    pub fn render(&self) -> String {
        format!(
            "{} from {} to {} [{}|{}|{}]",
            self.kind,
            self.source.render(),
            if self.target_name.is_empty() {
                "<empty>".to_owned()
            } else {
                self.target_name.clone()
            },
            self.state,
            self.evidence,
            match &self.target {
                None => "<unbound>".to_owned(),
                Some(row) => row.render(),
            }
        )
    }

    /// The evidence class, or an empty string for the states that carry none.
    fn evidence_of(state: &ResolutionState) -> &'static str {
        match state {
            ResolutionState::Pending { evidence, .. }
            | ResolutionState::Resolved {
                by: evidence, ..
            }
            | ResolutionState::Inferred {
                by: evidence, ..
            } => evidence.class(),
            ResolutionState::Ambiguous { .. } | ResolutionState::Unresolved { .. } => "",
        }
    }

    /// The import module and alias, in whichever decided state this relation is in.
    ///
    /// Mirrors the resolver's own `import_evidence`: `Ambiguous` and `Unresolved`
    /// carry no evidence at all, so an import that failed to resolve has no module
    /// left to check against. Those are counted and reported separately rather
    /// than being scored as a wrong module, because that would be measuring the
    /// resolution state model rather than import extraction.
    fn import_of(state: &ResolutionState) -> Option<(String, Option<String>)> {
        let evidence = match state {
            ResolutionState::Pending { evidence, .. }
            | ResolutionState::Resolved {
                by: evidence, ..
            }
            | ResolutionState::Inferred {
                by: evidence, ..
            } => evidence,
            _ => return None,
        };
        match evidence {
            Evidence::ImportBinding { module, alias } => Some((module.clone(), alias.clone())),
            _ => None,
        }
    }
}

/// Everything the gate reads out of one index.
#[derive(Debug, Default)]
pub struct Graph {
    rows: Vec<EntityRow>,
    ids: Vec<EntityId>,
    pub relations: Vec<RelationRow>,
    /// Entity indices keyed by `path | kind | qualified_name`.
    by_key: BTreeMap<String, Vec<usize>>,
    /// Keys with at least one incoming structural edge.
    defined: BTreeSet<String>,
}

impl Graph {
    pub fn read(store: &Store) -> Graph {
        let mut graph = Graph::default();
        for path in store.indexed_paths(usize::MAX).expect("indexed paths") {
            let entities = store
                .entities_in_file(&path, usize::MAX)
                .expect("entities in file");
            for entity in entities {
                graph.rows.push(row_of(entity.id()));
                graph.ids.push(entity.id().clone());
            }
        }
        let mut ordered: Vec<(EntityRow, EntityId)> = graph
            .rows
            .iter()
            .cloned()
            .zip(graph.ids.iter().cloned())
            .collect();
        ordered.sort_by(|left, right| left.0.cmp(&right.0));
        graph.rows = ordered.iter().map(|(row, _)| row.clone()).collect();
        graph.ids = ordered.into_iter().map(|(_, id)| id).collect();

        for state in all_states() {
            for relation in store
                .relations_in_state(&state, usize::MAX)
                .expect("relations in state")
            {
                let row = flatten(relation);
                if matches!(row.kind, "contains" | "defines" | "owns")
                    && let Some(target) = &row.target
                {
                    graph.defined.insert(target.key().render());
                }
                graph.relations.push(row);
            }
        }
        graph.relations.sort_by(|left, right| left.render().cmp(&right.render()));

        for (index, row) in graph.rows.iter().enumerate() {
            graph.by_key.entry(row.key().render()).or_default().push(index);
        }
        graph
    }

    pub fn entities(&self) -> &[EntityRow] {
        &self.rows
    }


    /// The identity a question can name, taken from the store rather than rebuilt.
    ///
    /// An `EntityId` carries an ordinal, and a qualified name does not. Where a
    /// source declares the same identity twice the lowest ordinal is used, and
    /// the symbol dimension scores the multiplicity rather than hiding it.
    pub fn find(&self, key: &Key) -> Option<&EntityId> {
        self.by_key
            .get(&key.render())
            .and_then(|found| found.first())
            .map(|index| &self.ids[*index])
    }

    /// Whether an incoming structural edge points at this identity.
    pub fn is_defined(&self, key: &Key) -> bool {
        self.defined.contains(&key.render())
    }

    /// Counts by relation class, then by state.
    pub fn class_states(&self) -> BTreeMap<String, BTreeMap<String, u64>> {
        let mut counts: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        for relation in &self.relations {
            *counts
                .entry(relation.kind.to_owned())
                .or_default()
                .entry(relation.state.to_owned())
                .or_default() += 1;
        }
        counts
    }

    pub fn entity_counts(&self) -> BTreeMap<String, u64> {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for row in &self.rows {
            *counts.entry(row.kind.clone()).or_default() += 1;
        }
        counts
    }

    /// Relations of the given kinds whose source is at `path` + `qualified_name`.
    ///
    /// A list of kinds rather than one, because the model declares both `owns`
    /// and `contains` and the extractor only ever emits the second. Which of them
    /// actually carries the edge is part of what the gate reports.
    pub fn from(&self, kinds: &[&str], path: &str, qualified_name: &str) -> Vec<&RelationRow> {
        self.relations
            .iter()
            .filter(|relation| {
                kinds.contains(&relation.kind)
                    && relation.source.path == path
                    && relation.source.qualified_name == qualified_name
            })
            .collect()
    }
}

/// Entity kinds the graph uses for repository structure rather than for a
/// declaration the fixture can label.
///
/// `module` is deliberately **not** here. The Rust spec maps both `impl_item` and
/// `mod_item` to `EntityKind::Module`, and `spec::module_nodes` is what tells them
/// apart, so excluding every module would throw away every `impl` block and every
/// inline module. What distinguishes the file-to-namespace module is its spelling:
/// a declaration's qualified name is built from the scope stack and joined with
/// `.`, while a module-layout name is joined with the language's own `::`. That is
/// an invariant of the model rather than a heuristic, and
/// `module_qualified_names_use_the_source_separator` pins it against the fixture.
pub fn is_structural(kind: &str, qualified_name: &str) -> bool {
    matches!(kind, "repository" | "workspace" | "package" | "file")
        || (kind == "module" && qualified_name.contains("::"))
}

/// One stand-in per resolution state.
///
/// The payload is irrelevant: `relations_in_state` filters on the stored tag, so a
/// stand-in with the right variant selects the whole bucket. This is the same
/// construction `real_repository.rs` uses for `Pending`.
pub fn all_states() -> [ResolutionState; 5] {
    [
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
            reason: UnresolvedReason::Ambiguous,
        },
    ]
}

fn flatten(relation: Relation) -> RelationRow {
    let imported = RelationRow::import_of(&relation.resolution);
    RelationRow {
        kind: relation.kind.as_str(),
        state: state_tag(&relation.resolution),
        evidence: RelationRow::evidence_of(&relation.resolution),
        source: row_of(&relation.source),
        target_name: relation.target_name.clone(),
        target: relation.target.as_ref().map(row_of),
        module: imported.as_ref().map(|(module, _)| module.clone()),
        alias: imported.and_then(|(_, alias)| alias),
    }
}

fn state_tag(state: &ResolutionState) -> &'static str {
    match state {
        ResolutionState::Pending { .. } => "pending",
        ResolutionState::Resolved { .. } => "resolved",
        ResolutionState::Inferred { .. } => "inferred",
        ResolutionState::Ambiguous { .. } => "ambiguous",
        ResolutionState::Unresolved { .. } => "unresolved",
    }
}

// ---------------------------------------------------------------------------
// Scratch trees
// ---------------------------------------------------------------------------

static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A directory the gate owns and deletes.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub fn new(label: &str) -> Scratch {
        let unique = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "peek-gate-{}-{label}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        Scratch { path }
    }


    /// A child of this scratch holding a copy of `from`.
    ///
    /// Every copy sits at `<scratch>/gate-fixture`, and the scratch is different
    /// for each operation, so the directory above `src` — which is what the
    /// module layout is named after — spells the same package in every copy. Two
    /// copies at different depths would declare different packages and could never
    /// be compared.
    pub fn crate_copy(&self, from: &Path) -> PathBuf {
        let destination = self.path.join("gate-fixture");
        let _ = std::fs::remove_dir_all(&destination);
        copy_tree(from, &destination);
        destination
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the destination directory");
    let entries = std::fs::read_dir(from)
        .expect("read the fixture directory")
        .map(|entry| entry.expect("a directory entry").path())
        .collect::<Vec<_>>();
    for entry in entries {
        let name = entry.file_name().expect("an entry with a name").to_owned();
        let destination = to.join(&name);
        if entry.is_dir() {
            copy_tree(&entry, &destination);
        } else {
            std::fs::copy(&entry, &destination).expect("copy a fixture file");
        }
    }
}

// ---------------------------------------------------------------------------
// Indexing
// ---------------------------------------------------------------------------

/// Redirect every index this gate makes into a directory it owns.
///
/// The programmatic override rather than the environment variable, because the
/// environment is process-global and the test binary runs its tests concurrently.
/// A gate that wrote into a developer's real cache would be a gate nobody trusts.
pub fn install_index_root() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let unique = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("peek-gate-index-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create the index root");
        peek_core::store::paths::set_root_override(Some(root));
    });
}

pub fn build(root: &Path) -> (Store, IndexReport) {
    install_index_root();
    let mut store = indexer::open_store(root).expect("open the store for the fixture copy");
    let outcome = indexer::build_full(&mut store, root, DiscoveryOptions::default())
        .expect("a full build of the fixture");
    store.verify().expect("the index verifies after a full build");
    (store, outcome.report().clone())
}

pub fn refresh(store: &mut Store, root: &Path, paths: &[PathBuf]) -> IndexReport {
    let outcome = indexer::refresh(store, root, paths, &DiscoveryOptions::default())
        .expect("an incremental refresh of the fixture");
    outcome.report().clone()
}

/// The index's whole semantic state, as canonical strings.
///
/// Keyed by `EntityId` for entities and by the store's own `natural_key` for
/// relations. Spans are excluded from the relation key deliberately: the row
/// identity does not include them, so including them here would compare something
/// the store never promised to hold.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub entities: BTreeMap<String, ()>,
    pub relations: BTreeMap<String, ()>,
}

impl Snapshot {
    pub fn of(store: &Store) -> Snapshot {
        let mut snapshot = Snapshot::default();
        for path in store.indexed_paths(usize::MAX).expect("indexed paths") {
            for entity in store
                .entities_in_file(&path, usize::MAX)
                .expect("entities in file")
            {
                let id = entity.id();
                snapshot.entities.insert(
                    format!(
                        "{}|{}|{}|{}",
                        id.path().as_str(),
                        id.kind().as_str(),
                        id.qualified_name(),
                        id.ordinal()
                    ),
                    (),
                );
            }
        }
        for state in all_states() {
            for relation in store
                .relations_in_state(&state, usize::MAX)
                .expect("relations in state")
            {
                snapshot
                    .relations
                    .insert(format!("{:?}", relation.natural_key()), ());
            }
        }
        snapshot
    }

    pub fn rows(&self) -> u64 {
        (self.entities.len() + self.relations.len()) as u64
    }

    /// Rows one snapshot holds that the other does not, named rather than counted.
    pub fn differences(&self, other: &Snapshot) -> Vec<String> {
        let mut differences: Vec<String> = self
            .entities
            .keys()
            .filter(|key| !other.entities.contains_key(*key))
            .map(|key| format!("entity {key}"))
            .collect();
        differences.extend(
            other
                .entities
                .keys()
                .filter(|key| !self.entities.contains_key(*key))
                .map(|key| format!("entity {key}")),
        );
        differences.extend(
            self.relations
                .keys()
                .filter(|key| !other.relations.contains_key(*key))
                .map(|key| format!("relation {key}")),
        );
        differences.extend(
            other
                .relations
                .keys()
                .filter(|key| !self.relations.contains_key(*key))
                .map(|key| format!("relation {key}")),
        );
        differences.sort();
        differences
    }
}

/// Print every entity row and every relation row, for reading a regression.
///
/// Off unless `PEEK_GATE_DUMP` is set, because the point of the gate is the
/// summary and a wall of rows is what makes a summary get ignored. The reason it
/// exists at all is that "which rows did the engine emit" is the question every
/// investigation of a surprising number ends at, and answering it should not
/// require editing the gate.
pub fn dump(store: &Store, graph: &Graph) {
    println!("\n--- entity rows ---");
    for row in graph.entities() {
        println!("  {}", row.render());
    }
    println!("\n--- relation rows ---");
    for relation in &graph.relations {
        println!("  {}", relation.render());
    }
    println!("\n--- class states ---");
    for (class, states) in graph.class_states() {
        println!("  {class}: {states:?}");
    }
    let stats = store.stats().expect("stats");
    println!("\n--- store stats ---\n  {stats:?}");
}

// ---------------------------------------------------------------------------
// The dimensions
// ---------------------------------------------------------------------------

/// Measure every dimension for one corpus whose fixture has been indexed.
pub fn measure(
    corpus: &Corpus,
    graph: &Graph,
    report: IndexReport,
    incremental: Fraction,
    query: Fraction,
    context: Fraction,
) -> Measurement {
    let mut missing: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let mut spurious: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();

    let structural_entities = graph
        .entities()
        .iter()
        .filter(|row| is_structural(&row.kind, &row.qualified_name))
        .count() as u64;

    // -- symbols ------------------------------------------------------------
    let truth = corpus.symbol_multiset();
    let mut found = Multiset::new();
    for row in graph.entities() {
        if !is_structural(&row.kind, &row.qualified_name) {
            found.add(row.key().render());
        }
    }
    let symbols = Match::of(&truth, &found);
    record_gaps(&mut missing, &mut spurious, "symbol_precision", &truth, &found);

    // -- definitions --------------------------------------------------------
    // Scored over distinct identities rather than over lines: two `impl` blocks
    // for one type are one identity with two rows, and "does this symbol have a
    // definition edge" has the same answer twice.
    let distinct = corpus.distinct_symbols();
    missing.insert(
        "definitions",
        distinct
            .iter()
            .filter(|key| !graph.is_defined(key))
            .map(|key| key.render())
            .collect(),
    );
    let defined = distinct
        .iter()
        .filter(|key| graph.is_defined(key))
        .count() as u64;

    // -- calls --------------------------------------------------------------
    let call_truth = call_multiset(&corpus.calls);
    let mut call_found = Multiset::new();
    for relation in graph.relations.iter().filter(|row| row.kind == "calls") {
        call_found.add(format!(
            "{}|{}|{}|{}",
            relation.source.path,
            relation.source.kind,
            relation.source.qualified_name,
            relation.target_name
        ));
    }
    let calls = Match::of(&call_truth, &call_found);
    record_gaps(&mut missing, &mut spurious, "calls", &call_truth, &call_found);

    // -- references ---------------------------------------------------------
    // Matched on path, source qualified name and name — not on the source's kind.
    // A reference's identity is "this symbol used this name", and no labelled
    // reference in the fixtures is ambiguous in that sense.
    let reference_truth = labelled_multiset(&corpus.references);
    let mut reference_found = Multiset::new();
    for relation in graph
        .relations
        .iter()
        .filter(|row| row.kind == "references")
    {
        reference_found.add(format!(
            "{}|{}|{}",
            relation.source.path, relation.source.qualified_name, relation.target_name
        ));
    }
    let references = Match::of(&reference_truth, &reference_found);
    record_gaps(
        &mut missing,
        &mut spurious,
        "references",
        &reference_truth,
        &reference_found,
    );

    // Negative controls: a declaration's own name is not a use of that name.
    let mut negative_reference_violations = Vec::new();
    for control in &corpus.no_references {
        let violated = graph
            .from(&["references"], &control.path, &control.subject)
            .iter()
            .any(|relation| relation.target_name == control.object);
        if violated {
            negative_reference_violations.push(format!(
                "{} | {} | {}",
                control.path, control.subject, control.object
            ));
        }
    }
    missing.insert("negative_references", negative_reference_violations);

    // -- imports ------------------------------------------------------------
    let mut import_truth = Multiset::new();
    for import in &corpus.imports {
        import_truth.add(render_import(
            &import.path,
            &import.local,
            &import.module,
            import.alias.as_deref(),
        ));
    }
    let mut import_found = Multiset::new();
    let mut import_evidenceless = 0u64;
    for relation in graph.relations.iter().filter(|row| row.kind == "imports") {
        let Some(module) = relation.module.clone() else {
            import_evidenceless += 1;
            continue;
        };
        import_found.add(render_import(
            &relation.source.path,
            &relation.target_name,
            &module,
            relation.alias.as_deref(),
        ));
    }
    let imports = Match::of(&import_truth, &import_found);
    record_gaps(&mut missing, &mut spurious, "imports", &import_truth, &import_found);
    if import_evidenceless > 0 {
        missing
            .entry("imports")
            .or_default()
            .push(format!(
                "{import_evidenceless} import relation(s) carry no module, because their resolution \
                 state holds no evidence"
            ));
    }

    // -- members ------------------------------------------------------------
    let mut member_misses = Vec::new();
    let mut members_matched = 0u64;
    for member in &corpus.members {
        // `Owns` is declared in the model and never emitted by the extractor, so
        // `contains` is the edge the engine actually has and the one to score.
        let owned = graph
            .from(&["contains", "owns"], &member.path, &member.subject)
            .iter()
            .any(|relation| {
                relation.target.as_ref().is_some_and(|target| {
                    target.path == member.path && target.qualified_name == member.object
                })
            });
        if owned {
            members_matched += 1;
        } else {
            member_misses.push(format!(
                "{} | {} owns {}",
                member.path, member.subject, member.object
            ));
        }
    }
    missing.insert("members", member_misses);

    // -- inheritance --------------------------------------------------------
    // Two figures, because the two answers are different claims. `inheritance_base`
    // is "an edge naming this base was produced"; `inheritance_subject` is "and it
    // also names the type that declared it". A gate that reported only the first
    // would call an edge that points from the *file* a correct inheritance edge.
    let mut subject_truth = Multiset::new();
    let mut base_truth = Multiset::new();
    for clause in corpus.inherits.iter().chain(corpus.implements.iter()) {
        subject_truth.add(format!(
            "{}|{}|{}",
            clause.path, clause.subject, clause.object
        ));
        base_truth.add(format!("{}|{}", clause.path, clause.object));
    }
    let mut subject_found = Multiset::new();
    let mut base_found = Multiset::new();
    for relation in graph
        .relations
        .iter()
        .filter(|row| matches!(row.kind, "inherits" | "implements"))
    {
        subject_found.add(format!(
            "{}|{}|{}",
            relation.source.path, relation.source.qualified_name, relation.target_name
        ));
        base_found.add(format!("{}|{}", relation.source.path, relation.target_name));
    }
    let inheritance_subject = Match::of(&subject_truth, &subject_found);
    let inheritance_base = Match::of(&base_truth, &base_found);
    record_gaps(
        &mut missing,
        &mut spurious,
        "inheritance_subject",
        &subject_truth,
        &subject_found,
    );
    record_gaps(
        &mut missing,
        &mut spurious,
        "inheritance_base",
        &base_truth,
        &base_found,
    );

    // Negative controls: a type that names no base must carry no inheritance edge.
    let mut negative_inherit_violations = Vec::new();
    for control in &corpus.no_inherits {
        let violated = graph
            .relations
            .iter()
            .any(|relation| {
                matches!(relation.kind, "inherits" | "implements")
                    && relation.source.path == control.path
                    && relation.source.qualified_name == control.subject
            });
        if violated {
            negative_inherit_violations
                .push(format!("{} | {}", control.path, control.subject));
        }
    }
    missing.insert("negative_inheritance", negative_inherit_violations);

    let dimensions = vec![
        dimension("symbol_precision", symbols.precision()),
        dimension("symbol_recall", symbols.recall()),
        dimension("definitions", Fraction::new(defined, distinct.len() as u64)),
        dimension("calls", calls.recall()),
        dimension("references", references.recall()),
        dimension("imports", imports.recall()),
        dimension("members", Fraction::new(members_matched, corpus.members.len() as u64)),
        dimension("inheritance_subject", inheritance_subject.recall()),
        dimension("inheritance_base", inheritance_base.recall()),
        dimension(
            "negative_references",
            Fraction::new(
                corpus.no_references.len() as u64 - missing["negative_references"].len() as u64,
                corpus.no_references.len() as u64,
            ),
        ),
        dimension(
            "negative_inheritance",
            Fraction::new(
                corpus.no_inherits.len() as u64 - missing["negative_inheritance"].len() as u64,
                corpus.no_inherits.len() as u64,
            ),
        ),
        dimension("incremental", incremental),
        dimension("query", query),
        dimension("context", context),
    ];

    Measurement {
        language: corpus.language,
        dimensions,
        entity_counts: graph.entity_counts(),
        class_states: graph.class_states(),
        missing,
        spurious,
        query_failures: Vec::new(),
        context_failures: Vec::new(),
        index_report: report,
        structural_entities,
    }
}

impl Measurement {
    /// Attach the assertion failures the query and context checks recorded.
    #[must_use]
    pub fn with_failures(mut self, query: Vec<String>, context: Vec<String>) -> Measurement {
        self.query_failures = query;
        self.context_failures = context;
        self
    }
}

fn dimension(name: &'static str, value: Fraction) -> Dimension {
    Dimension { name, value }
}

fn call_multiset(calls: &[LabelledCall]) -> Multiset {
    let mut set = Multiset::new();
    for call in calls {
        set.add(format!(
            "{}|{}|{}|{}",
            call.path, call.kind, call.subject, call.object
        ));
    }
    set
}

fn labelled_multiset(labelled: &[Labelled]) -> Multiset {
    let mut set = Multiset::new();
    for item in labelled {
        set.add(format!("{}|{}|{}", item.path, item.subject, item.object));
    }
    set
}

fn render_import(path: &str, local: &str, module: &str, alias: Option<&str>) -> String {
    format!(
        "{}|{}|{}|{}",
        path,
        local,
        module,
        alias.unwrap_or("-")
    )
}

/// Name the labels the engine missed and the relations it produced that no label
/// accounts for. The counts are the score; these are the evidence for it.
fn record_gaps(
    missing: &mut BTreeMap<&'static str, Vec<String>>,
    spurious: &mut BTreeMap<&'static str, Vec<String>>,
    dimension: &'static str,
    truth: &Multiset,
    found: &Multiset,
) {
    let mut absent = Vec::new();
    let mut extra = Vec::new();
    for key in Multiset::merged_keys(truth, found) {
        let want = truth.count_of(&key);
        let got = found.count_of(&key);
        if want > got {
            absent.push(format!("{key} (labelled {want}, emitted {got})"));
        } else if got > want {
            extra.push(format!("{key} (labelled {want}, emitted {got})"));
        }
    }
    missing.insert(dimension, absent);
    spurious.insert(dimension, extra);
}

// ---------------------------------------------------------------------------
// Query correctness
// ---------------------------------------------------------------------------

/// Score the query surface against the fixture.
///
/// Two populations, both stated: the hand-written questions in the expectation
/// file, and an assertion derived from every labelled call the engine resolved —
/// `callees` of the caller must contain the callee and `callers` of the callee must
/// contain the caller. The derived half is what stops this being a handful of
/// hand-picked spot checks.
pub fn score_queries(corpus: &Corpus, store: &Store, graph: &Graph) -> (Fraction, Vec<String>) {
    let query = Query::new(store);
    let mut passed = 0u64;
    let mut total = 0u64;
    let mut failures = Vec::new();

    for call in &corpus.calls {
        let Some(callee) = resolved_callee(graph, call) else {
            continue;
        };
        let caller = Key::new(&call.path, &call.kind, &call.subject);
        let (Some(caller), Some(callee)) = (graph.find(&caller), graph.find(&callee)) else {
            continue;
        };

        total += 1;
        let outbound = query
            .callees(caller)
            .map(|walk| walk.contains(callee))
            .unwrap_or(false);
        if outbound {
            passed += 1;
        } else {
            failures.push(format!(
                "callees({}) does not contain {}",
                caller.qualified_name(),
                callee.qualified_name()
            ));
        }

        total += 1;
        let inbound = query
            .callers(callee)
            .map(|walk| walk.contains(caller))
            .unwrap_or(false);
        if inbound {
            passed += 1;
        } else {
            failures.push(format!(
                "callers({}) does not contain {}",
                callee.qualified_name(),
                caller.qualified_name()
            ));
        }
    }

    for (label, pairs, request) in [
        ("callers", &corpus.callers, WalkRequest::callers()),
        ("callees", &corpus.callees, WalkRequest::callees()),
        (
            "implementations",
            &corpus.implementations,
            WalkRequest {
                direction: Direction::Inbound,
                depth: 1,
                kind: Some(RelationKind::Implements),
            },
        ),
    ] {
        for pair in pairs.iter() {
            total += 1;
            if answer_contains(&query, graph, pair, request) {
                passed += 1;
            } else {
                failures.push(format!(
                    "{label}({}) does not contain {}",
                    pair.target.qualified_name,
                    pair.expected.render()
                ));
            }
        }
    }

    (Fraction::new(passed, total), failures)
}

fn answer_contains(
    query: &Query<'_>,
    graph: &Graph,
    pair: &LabelledPair,
    request: WalkRequest,
) -> bool {
    let Some(target) = graph.find(&pair.target) else {
        return false;
    };
    query
        .walk(target, request)
        .map(|walk| {
            walk.steps
                .iter()
                .any(|step| render_id(&step.id) == pair.expected.render())
        })
        .unwrap_or(false)
}

fn resolved_callee(graph: &Graph, call: &LabelledCall) -> Option<Key> {
    graph
        .from(&["calls"], &call.path, &call.subject)
        .into_iter()
        .find(|relation| relation.target_name == call.object)
        .and_then(|relation| relation.target.as_ref())
        .map(|target| Key::new(&target.path, &target.kind, &target.qualified_name))
}

// ---------------------------------------------------------------------------
// Context quality
// ---------------------------------------------------------------------------

/// Score `peek` against the fixture: a question, and the entity the answer has to
/// name for the answer to be any use.
pub fn score_context(corpus: &Corpus, store: &Store) -> (Fraction, Vec<String>) {
    let query = Query::new(store);
    let budget = 20_000u64;
    let mut passed = 0u64;
    let mut total = 0u64;
    let mut failures = Vec::new();

    for context in &corpus.contexts {
        total += 1;
        match query.peek(&context.question, budget) {
            Ok(pack) => {
                let named_expected = pack
                    .unit_ids()
                    .into_iter()
                    .any(|id| render_id(id) == context.expected.render());
                if named_expected {
                    passed += 1;
                } else {
                    let names: Vec<String> =
                        pack.unit_ids().into_iter().map(render_id).collect();
                    failures.push(format!(
                        "peek({:?}) did not name {}; it named {names:?}",
                        context.question,
                        context.expected.render()
                    ));
                }
            }
            Err(error) => failures.push(format!(
                "peek({:?}) failed: {error}",
                context.question
            )),
        }
    }

    for question in &corpus.ambiguous_contexts {
        total += 1;
        match query.peek(question, budget) {
            // E4's second gate, in the only form that can fail: a name with two
            // declarations must be refused with both candidates rather than
            // resolved to whichever came first.
            Err(QueryError::AmbiguousTarget { candidates, .. }) if candidates.len() >= 2 => {
                passed += 1;
            }
            Ok(pack) => failures.push(format!(
                "peek({question:?}) resolved an ambiguous name to {} and offered {} candidates; \
                 it must refuse",
                pack.target.id().qualified_name(),
                pack.unit_ids().len()
            )),
            Err(error) => failures.push(format!(
                "peek({question:?}) must refuse an ambiguous name and offer its candidates; it \
                 returned {error}"
            )),
        }
    }

    (Fraction::new(passed, total), failures)
}