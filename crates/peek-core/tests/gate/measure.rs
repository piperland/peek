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

use super::expect::{Bind, Corpus, Key, Labelled, LabelledCall, LabelledPair};
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
    "resolution_correctness",
    "imports",
    "imports_module_retained",
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
    /// Decided relations that point somewhere other than the labelled entity,
    /// and the labelled ones left undecided. Kept apart from [`Self::missing`]
    /// because they are different failures with different remedies: a gap is fixed
    /// by extracting more, and a wrong edge by resolving differently or not at
    /// all. Merging them into one figure would hide the second behind the first.
    pub placement: Placement,
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
    /// The relation's first byte in its file.
    ///
    /// Carried so a measurement can ask a **syntactic** question about the row — what
    /// the occurrence is, and what binds it — by joining on the file rather than on the
    /// name. A name is not enough: `Entry.label` and `entry.label` are two declarations
    /// of one name, and the placement dimensions tell them apart by qualified name
    /// while a question about the source can only be told by going back to the byte.
    pub start_byte: u32,
}

impl RelationRow {
    /// A stable identity for counting, independent of row ordering.
    pub fn render(&self) -> String {
        format!(
            "{} from {} to {} [{}|{}|{}{}]",
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
            },
            // The module and alias an import names are the whole provenance of the
            // binding, and a state that does not hold them shows up here as
            // nothing rather than as a wrong value — which is the finding.
            match &self.module {
                None => String::new(),
                Some(module) => format!(
                    " module={module} alias={}",
                    self.alias.clone().unwrap_or_else(|| "-".to_owned())
                ),
            }
        )
    }

    /// Whether the engine committed to a target: the two states that name one.
    ///
    /// **`Pending` is not one of them.** A pending relation is awaiting a decision
    /// and carries a target only as an aspiration, so scoring it as a placement
    /// would count an intention as an answer — and would make `Pending` and
    /// `Unresolved` read alike, when one is work outstanding and the other is a
    /// rung that declined to guess.
    pub fn is_decided(&self) -> bool {
        matches!(self.state, "resolved" | "inferred")
    }

    /// The same relation, written the way a reader wants to hear about a wrong
    /// edge: what it names, where it is written, and where it lands.
    ///
    /// The rung is in the string because "wrong" and "wrong by the weakest rung
    /// available" are different diagnoses, and four edges that look identical in a
    /// count are not the same defect.
    pub fn describe_decided(&self) -> String {
        format!(
            "{} from {} | {} names `{}` and the {} rung placed it on {}",
            self.kind,
            self.source.path,
            self.source.qualified_name,
            self.target_name,
            self.evidence,
            match &self.target {
                None => "<nothing>".to_owned(),
                Some(row) => row.render(),
            }
        )
    }

    /// The evidence class, or an empty string for the states that carry none.
    fn evidence_of(state: &ResolutionState) -> &'static str {
        match state {
            ResolutionState::Pending { evidence, .. }
            | ResolutionState::Resolved { by: evidence, .. }
            | ResolutionState::Inferred { by: evidence, .. } => evidence.class(),
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
            | ResolutionState::Resolved { by: evidence, .. }
            | ResolutionState::Inferred { by: evidence, .. } => evidence,
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
    /// Row indices that describe repository structure rather than a declaration.
    ///
    /// A file-layout module is told from a declared one by **who contains it**, not
    /// by how its name is spelled: the layout module is contained by the `File`
    /// entity, and a `mod x { .. }` or an `impl` block is contained by its parent
    /// scope. That is a property of the graph rather than a guess about separators,
    /// and it is what lets both kinds stay in the measured population while the
    /// layout modules stay out of it.
    structural: BTreeSet<usize>,
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
        graph.relations.sort_by_key(RelationRow::render);

        for (index, row) in graph.rows.iter().enumerate() {
            graph
                .by_key
                .entry(row.key().render())
                .or_default()
                .push(index);
            if is_a_structure_kind(&row.kind) {
                graph.structural.insert(index);
            }
        }

        // A module the file layout created is contained by the file. A `mod x { .. }`
        // declaration and an `impl` block are contained by their parent scope, and
        // the walker gives both the same `EntityKind::Module`, so containment is the
        // only thing in the graph that tells them apart.
        let contained_by_a_file: BTreeSet<(String, String, String)> = graph
            .relations
            .iter()
            .filter(|relation| relation.kind == "contains" && relation.source.kind == "file")
            .filter_map(|relation| {
                relation.target.as_ref().map(|target| {
                    (
                        target.path.clone(),
                        target.kind.clone(),
                        target.qualified_name.clone(),
                    )
                })
            })
            .collect();
        for (index, row) in graph.rows.iter().enumerate() {
            let key = (
                row.path.clone(),
                row.kind.clone(),
                row.qualified_name.clone(),
            );
            if row.kind == "module" && contained_by_a_file.contains(&key) {
                graph.structural.insert(index);
            }
        }
        graph
    }

    /// Whether this row describes repository structure rather than a declaration.
    ///
    /// Held out of the symbol denominator and counted separately, because a file's
    /// own module is not something the fixture labels and a fixture that labelled
    /// it would be labelling the harness's own layout rather than the language.
    pub fn is_structural(&self, index: usize) -> bool {
        self.structural.contains(&index)
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

    /// Every entity carrying one declared name anywhere in the index.
    ///
    /// The population R5 asks about, read the same way R5 asks about it: by the
    /// declared name, across every file. Written as `rsplit('.')` on the qualified
    /// name because that is how a declaration's own name is written there —
    /// `Entry.count` declares `count` — and because reading it any other way would
    /// answer a different question than the one the rung asks.
    pub fn named(&self, name: &str) -> Vec<&EntityRow> {
        self.rows
            .iter()
            .filter(|row| row.qualified_name.rsplit('.').next() == Some(name))
            .collect()
    }

    /// Whether the entity at `key` exists in this index.
    ///
    /// Asked before anything is said about a label, because the answer separates
    /// an engine finding from a fixture one. A label naming an entity the index
    /// does not hold cannot be satisfied by any rung, so a failure to satisfy it
    /// says the label is wrong and not that the engine placed the edge wrongly.
    pub fn holds(&self, key: &Key) -> bool {
        self.by_key.contains_key(&key.render())
    }
}

/// Entity kinds the graph uses for repository structure rather than for a
/// declaration a fixture can label.
fn is_a_structure_kind(kind: &str) -> bool {
    matches!(kind, "repository" | "workspace" | "package" | "file")
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
        start_byte: relation.span.start_byte,
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
        let path =
            std::env::temp_dir().join(format!("peek-gate-{}-{label}-{unique}", std::process::id()));
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
        let root =
            std::env::temp_dir().join(format!("peek-gate-index-{}-{unique}", std::process::id()));
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
    store
        .verify()
        .expect("the index verifies after a full build");
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

    let structural_entities = graph.structural.len() as u64;

    // -- symbols ------------------------------------------------------------
    let truth = corpus.symbol_multiset();
    let mut found = Multiset::new();
    for (index, row) in graph.entities().iter().enumerate() {
        if !graph.is_structural(index) {
            found.add(row.key().render());
        }
    }
    let symbols = Match::of(&truth, &found);
    record_gaps(
        &mut missing,
        &mut spurious,
        "symbol_precision",
        &truth,
        &found,
    );

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
    let defined = distinct.iter().filter(|key| graph.is_defined(key)).count() as u64;

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
    record_gaps(
        &mut missing,
        &mut spurious,
        "calls",
        &call_truth,
        &call_found,
    );

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

    // -- placement: does a decided edge point at the right entity -------------
    // A different question from the one above, and the one the other dimensions
    // cannot ask. An edge can exist, be the right edge, and still name the wrong
    // entity: `summarise -> out` is a real use of a name that no declaration
    // anywhere carries, and placing it on `render.out` turns a missing edge into a
    // claim. The wrong edges are reported by name, and the undecided ones are
    // counted beside them rather than scored as failures.
    let placement = score_placement(corpus, graph);

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
    record_gaps(
        &mut missing,
        &mut spurious,
        "imports",
        &import_truth,
        &import_found,
    );
    if import_evidenceless > 0 {
        missing.entry("imports").or_default().push(format!(
            "{import_evidenceless} import relation(s) carry no module, because their resolution \
                 state holds no evidence"
        ));
    }

    // -- imports: does the module survive resolution? -------------------------
    // A separate question from whether the binding was extracted at all. An
    // `Ambiguous` or `Unresolved` relation cannot answer it, and neither can one the
    // resolver placed by a rung that replaced the import binding with weaker
    // evidence — so this counts over the labelled imports only, and the misses say
    // which state each one ended in.
    let mut module_retained = 0u64;
    let mut module_lost: Vec<String> = Vec::new();
    for import in &corpus.import_no_modules {
        let bound = graph.relations.iter().find(|relation| {
            relation.kind == "imports"
                && relation.source.path == import.path
                && relation.target_name == import.subject
        });
        match bound {
            Some(relation) if relation.module.is_some() => module_retained += 1,
            Some(relation) => module_lost.push(format!(
                "{} | {} -> {} [{}]",
                import.path, import.subject, relation.target_name, relation.state
            )),
            None => module_lost.push(format!(
                "{} | {} -> no import relation at all",
                import.path, import.subject
            )),
        }
    }
    missing.insert("imports_module_retained", module_lost);

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
        let violated = graph.relations.iter().any(|relation| {
            matches!(relation.kind, "inherits" | "implements")
                && relation.source.path == control.path
                && relation.source.qualified_name == control.subject
        });
        if violated {
            negative_inherit_violations.push(format!("{} | {}", control.path, control.subject));
        }
    }
    missing.insert("negative_inheritance", negative_inherit_violations);

    let dimensions = vec![
        dimension("symbol_precision", symbols.precision()),
        dimension("symbol_recall", symbols.recall()),
        dimension("definitions", Fraction::new(defined, distinct.len() as u64)),
        dimension("calls", calls.recall()),
        dimension("references", references.recall()),
        dimension("resolution_correctness", placement.fraction()),
        dimension("imports", imports.recall()),
        dimension(
            "imports_module_retained",
            Fraction::new(module_retained, corpus.import_no_modules.len() as u64),
        ),
        dimension(
            "members",
            Fraction::new(members_matched, corpus.members.len() as u64),
        ),
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
        placement,
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

// ---------------------------------------------------------------------------
// Placement: does a decided edge point at the right entity
// ---------------------------------------------------------------------------

/// What one decided relation got right, or what it got wrong.
#[derive(Debug, Clone)]
pub struct Placement {
    /// Decided relations that point at the entity the fixture names.
    pub correct: u64,
    /// Every decided relation carrying a placement label. The denominator.
    pub decided: u64,
    /// Labelled relations the engine left undecided: a gap, not a wrong edge.
    pub undecided: Vec<String>,
    /// Decided relations pointing somewhere else, as `(label key, description)`.
    ///
    /// The key is carried beside the sentence rather than parsed back out of it,
    /// because this is the one place where a measurement has to line up with
    /// another measurement row by row — the reachability verdict for an edge is
    /// only meaningful if it is the verdict for *that* edge, and matching two
    /// prose strings by whether one contains the other is how a reading gets
    /// attached to the wrong row.
    ///
    /// **One entry per labelled site, not per edge.** Deduplicated by key, so a source
    /// that writes the same name three times contributes one line however many of its
    /// rows are wrong. [`Self::wrong_edges`] is the count over edges; the two are
    /// different populations and only one of them is comparable with
    /// [`Self::decided`].
    pub wrong: Vec<(String, String)>,
    /// Decided relations pointing somewhere else, counted as **edges**.
    ///
    /// Published beside [`Self::wrong`] because the two do not agree and a reader who
    /// takes one for the other is wrong by a factor of two: on the Rust fixture
    /// `decided - correct` is 12 while `wrong` lists 6 sites, and the matrix published
    /// "6 decided and wrong" beside "48 decided". **Counted rather than asserted** —
    /// `decided - correct == wrong_edges` is an identity, and an assertion of an identity
    /// is a check that cannot fail, which is worse than no check because it is evidence.
    /// The number is printed so the reader can do the subtraction.
    pub wrong_edges: u64,
    /// Placement labels with no matching relation at all, each named.
    pub absent: Vec<String>,
    /// Of the fixture's labelled relations of a scored class, how many carry a
    /// placement claim. Published, because a label with no claim is a relation
    /// whose placement nobody is checking.
    pub covered: u64,
    /// The labelled relations of a scored class, counted with multiplicity.
    pub labeled: u64,
    /// Whether each labelled relation's correct entity was reachable by any rung.
    /// The measurement that decides whether a wrong edge is an extractor or a
    /// resolver defect, taken over the whole population.
    pub reach: Vec<Reach>,
}

impl Placement {
    /// The fraction of decided relations that are right.
    ///
    /// **The denominator is the decided relations, not the labelled ones.** That is
    /// the whole point of the column: it is the population the engine claims to
    /// have answered, so every member of it is a claim being checked. Scoring over
    /// the labelled population instead would let the engine raise the figure by
    /// deciding *less* — the exact move this column exists to make unattractive.
    /// The undecided count is therefore reported beside it and never folded in.
    pub fn fraction(&self) -> Fraction {
        Fraction::new(self.correct, self.decided)
    }
}

/// **For each decided relation, is the correct entity reachable by any rung at
/// all?** This is the question that separates the two explanations of a wrong
/// edge, and it is asked from the index rather than from the source.
///
/// * **Extractor.** The relation names something that is not a name in this
///   language — a `let` binding, a field read whose owner the extractor dropped.
///   There is no entity the edge could point at, so **no rung could have placed
///   it correctly**, and the correct outcome is `Unresolved`. The rung that
///   answered did not lie: it was asked a question with one candidate.
/// * **Resolver.** The correct entity is in the index and is reachable by a rung
///   the relation's own evidence would have let fire. The material was in hand and
///   the ladder reached for the wrong rung, or the wrong rung answered when a
///   stronger one had something to say.
///
/// The test is deliberately mechanical: `graph.named(name)` is the population R5
/// asks about, and `contained_by` is the `contains` chain that tells whether the
/// source's own scope had already declared the name. A judgement made by reading
/// the resolver's source would be an inference about intent; this is a count
/// against the graph the engine actually built.
#[derive(Debug, Clone)]
pub struct Reach {
    /// The label key the reading is about.
    pub key: String,
    /// Every entity in the index whose declared name is the one named.
    pub carriers: Vec<String>,
    /// The entity the label says the relation must point at, or `None` when the
    /// label says no entity is the referent.
    pub wants: Option<Key>,
    /// The name the relation is about, as the relation spells it. Kept because an
    /// aliased binding is spelled differently from the declaration it names, and
    /// that difference is the whole of one of the verdicts.
    pub name: String,
    /// Whether the index holds the entity the label names at all.
    pub target_in_index: bool,
    /// Whether any of the carriers is the entity the label names.
    pub correct_in_carriers: bool,
    /// Whether the symbol the relation is written in already declares that name
    /// itself.
    ///
    /// This is the sharpest fact the graph holds, because it is the one that
    /// distinguishes *a declaration that was in front of the rung* from *a name
    /// that only happens to exist elsewhere*: `describe` declares a parameter
    /// called `entry` and also imports a function called `entry`, and an edge on
    /// that name can only mean one of them.
    ///
    /// **Containment, and not the source's own qualified name.** A qualified name
    /// says what a declaration is called; containment says what is inside it, and
    /// the difference is the whole finding — `describe`'s parameter is in the same
    /// file as the import that beat it, so a same-file rung had it in hand.
    pub shadowed_in_source: bool,
}

impl Reach {
    /// Which explanation the measurement supports.
    ///
    /// Four cases, and **none of them panics**. A measurement that can abort on
    /// an unexpected fixture is a measurement that has been fitted to one fixture:
    /// the next label it has not seen becomes a crash rather than a number, and a
    /// crash in the middle of a gate run is a failure with no reading in it. Every
    /// input produces a sentence.
    pub fn verdict(&self) -> String {
        match &self.wants {
            // The label says the name has no referent, so the relation is
            // legitimate and no target is. A rung that answered anyway placed it
            // on something the source never referred to, and the correct answer
            // was `Unresolved`. Whatever carried the name, the rung could not
            // have known it was the wrong one: nothing in the index says the name
            // is a binding rather than a declaration.
            None => "extractor: the name denotes no declaration, so no rung could have placed it"
                .to_owned(),
            // The label names an entity the index does not hold. Nothing to say
            // about the engine: the label is what has to be checked.
            Some(want) if !self.target_in_index => {
                format!("label: `{want}` is not in this index, so the claim cannot be scored")
            }
            // The target is in the index but declares a different name than the
            // relation names. That is what an import alias is, and it is the one
            // shape a bare-name rung cannot reach by construction.
            Some(want) if declared_name(&want.qualified_name) != self.name => format!(
                "resolver: `{want}` declares `{}` and the relation names `{}`, so only the \
                 import rung can reach it",
                declared_name(&want.qualified_name),
                self.name
            ),
            // The source's own scope declares the name, so a weaker rung answered
            // over the declaration that was in front of it.
            Some(_) if self.shadowed_in_source => {
                "resolver: the source's own scope declares the name, and a weaker rung answered"
                    .to_owned()
            }
            Some(_) => "resolver: the correct entity is a candidate by name".to_owned(),
        }
    }
}

/// Measure, for every labelled relation, whether its correct entity was reachable.
///
/// Run over the whole labelled population rather than only over the wrong edges,
/// because a reachability measure that is computed after the fact is a
/// post-hoc explanation and not a measurement: a verdict about the wrong edges
/// means something only if the same procedure says `reachable` for the ones that
/// came out right.
pub fn measure_reach(corpus: &Corpus, graph: &Graph) -> Vec<Reach> {
    corpus
        .binds
        .iter()
        .map(|bind| {
            let carriers: Vec<String> = graph
                .named(&bind.name)
                .into_iter()
                .map(EntityRow::render)
                .collect();
            let correct_in_carriers = bind.target.as_ref().is_some_and(|target| {
                graph
                    .named(&bind.name)
                    .iter()
                    .any(|row| row.key() == *target)
            });
            Reach {
                key: bind.key(),
                carriers,
                wants: bind.target.clone(),
                name: bind.name.clone(),
                target_in_index: bind
                    .target
                    .as_ref()
                    .is_some_and(|target| graph.holds(target)),
                correct_in_carriers,
                shadowed_in_source: declares_in_scope(graph, bind),
            }
        })
        .collect()
}

/// Whether the symbol a relation is written in declares that name itself.
///
/// Through the `contains` chain, because containment is the only thing in the
/// graph that distinguishes `describe.entry` from `model.rs entry`: both are a
/// `parameter`/`function` pair with the same bare name, and only one of them is
/// inside the symbol whose body the relation is written in.
fn declares_in_scope(graph: &Graph, bind: &Bind) -> bool {
    let source = Key::new(&bind.path, &bind.kind, &bind.subject);
    graph
        .relations
        .iter()
        .filter(|relation| {
            matches!(relation.kind, "contains" | "defines")
                && relation.source.key() == source
                && relation
                    .target
                    .as_ref()
                    .is_some_and(|target| declared_name(&target.qualified_name) == bind.name)
        })
        .any(|relation| {
            relation
                .target
                .as_ref()
                .is_some_and(|target| target.kind != "file" && target.kind != "module")
        })
}

/// The bare name a qualified name ends in, which is what an occurrence of it in
/// a body means.
fn declared_name(qualified_name: &str) -> &str {
    qualified_name.rsplit('.').next().unwrap_or(qualified_name)
}

/// Score placement over every labelled relation that carries a `binds` label.
///
/// Matched on class, path, **source kind**, subject and name. The kind is in the
/// key because `counted` is both a macro and a function in the Rust fixture and a
/// relation written in one is not a relation written in the other; the name is in
/// it because a reference to `count` and a call to `count` from the same function
/// are two relations with two answers, and scoring them as one would report
/// whichever was checked last.
pub fn score_placement(corpus: &Corpus, graph: &Graph) -> Placement {
    let mut correct = 0u64;
    let mut decided = 0u64;
    // Counted before the list is deduplicated, because deduplication is what makes the
    // two figures disagree and the size of the disagreement is the finding.
    let mut wrong_here_total = 0u64;
    let mut undecided = Vec::new();
    let mut wrong = Vec::new();
    let mut absent = Vec::new();

    for bind in &corpus.binds {
        let rows: Vec<&RelationRow> = graph
            .relations
            .iter()
            .filter(|relation| {
                relation.kind == bind.class
                    && relation.source.path == bind.path
                    && relation.source.kind == bind.kind
                    && relation.source.qualified_name == bind.subject
                    && relation.target_name == bind.name
            })
            .collect();

        if rows.is_empty() {
            absent.push(format!(
                "{} | {} | {} names `{}`, and no relation does",
                bind.class, bind.path, bind.subject, bind.name
            ));
            continue;
        }

        // Every row for one label has to agree. A source that uses a name twice
        // produces two rows, and scoring the first and ignoring the second would
        // report the engine right on the strength of one of two answers.
        let mut right = 0u64;
        let mut claimed = 0u64;
        let mut wrong_here = 0u64;
        for row in &rows {
            if !row.is_decided() {
                undecided.push(format!(
                    "{} from {} | {} names `{}` [{}]",
                    bind.class, row.source.path, row.source.qualified_name, bind.name, row.state
                ));
                continue;
            }
            claimed += 1;
            if placement_holds(bind.target.as_ref(), row.target.as_ref()) {
                right += 1;
            } else {
                wrong_here += 1;
                wrong.push((
                    bind.key(),
                    format!(
                        "{}; the label says {}",
                        row.describe_decided(),
                        match &bind.target {
                            None => "no entity is the referent".to_owned(),
                            Some(key) => format!("`{}`", key.render()),
                        }
                    ),
                ));
            }
        }
        decided += claimed;
        correct += right;
        wrong_here_total += wrong_here;
    }

    undecided.sort();
    undecided.dedup();
    let wrong_edges = wrong_here_total;
    wrong.sort();
    wrong.dedup_by_key(|(key, _)| key.clone());
    absent.sort();
    let (covered, labeled) = placement_coverage(corpus);
    Placement {
        correct,
        decided,
        undecided,
        wrong,
        wrong_edges,
        absent,
        covered,
        labeled,
        reach: measure_reach(corpus, graph),
    }
}

/// How much of the labelled population the placement labels actually cover.
///
/// **Counted, not asserted.** A labelled relation with no `binds` line is a
/// relation whose placement the gate does not check, which is a fact about the
/// gate's coverage rather than a defect in the engine; an assertion here could
/// only fail once every fixture were complete, and would make the check that
/// fires on an engine change depend on an editorial decision about the fixture.
fn placement_coverage(corpus: &Corpus) -> (u64, u64) {
    let mut labeled = 0u64;
    let mut covered = 0u64;
    let placed: BTreeMap<String, u64> = corpus.binds.iter().fold(
        BTreeMap::new(),
        |mut counts: BTreeMap<String, u64>, bind: &Bind| {
            *counts
                .entry(site_of(
                    bind.class.as_str(),
                    &bind.path,
                    &bind.subject,
                    &bind.name,
                ))
                .or_default() += 1;
            counts
        },
    );
    for (class, path, subject, name) in labelled_sites(corpus) {
        labeled += 1;
        if placed
            .get(&site_of(class, path, subject, name))
            .is_some_and(|claims| *claims > 0)
        {
            covered += 1;
        }
    }
    (covered, labeled)
}

/// One labelled relation, as the four fields a placement claim is matched on.
///
/// **The source kind is deliberately absent.** A `reference` line does not carry
/// one — a use of a name is not a call and the model does not need the caller's
/// kind to say which relation it meant — so requiring a kind here would make the
/// coverage figure depend on a field one of the three populations does not have.
/// Two relations from the same symbol naming the same name are the same site
/// whichever kind the symbol is, and `score_placement` still checks every row.
fn site_of(class: &str, path: &str, subject: &str, name: &str) -> String {
    format!("{class}|{path}|{subject}|{name}")
}

/// Every relation the fixture's existence labels claim, as `(class, path, subject,
/// name)`.
///
/// `reference` lines carry no kind — a use of a name is not a call and the model
/// does not need the caller's kind to say which relation was meant — so coverage
/// is matched on the four fields all three populations have. `score_placement`
/// still keys on five, and a `binds` line with the wrong kind is caught there as
/// an absent relation rather than being silently scored as coverage.
fn labelled_sites(corpus: &Corpus) -> Vec<(&str, &str, &str, &str)> {
    let mut sites: Vec<(&str, &str, &str, &str)> = Vec::new();
    for reference in &corpus.references {
        sites.push((
            "references",
            &reference.path,
            &reference.subject,
            &reference.object,
        ));
    }
    for call in &corpus.calls {
        sites.push(("calls", &call.path, &call.subject, &call.object));
    }
    for import in &corpus.imports {
        // An import relation's source is the file entity, and a file entity's
        // qualified name is the file's own name. A fact about the model rather
        // than a guess about the fixture.
        if let Some(file) = Path::new(&import.path).file_name() {
            sites.push((
                "imports",
                &import.path,
                file.to_str().unwrap_or(""),
                &import.local,
            ));
        }
    }
    sites
}

/// Whether one placed target satisfies one placement label.
///
/// A label naming no entity is satisfied **only** by a relation that named none,
/// which cannot happen for a decided relation — so every decided relation under a
/// `binds_nothing` line is a wrong edge, and that is the intended reading rather
/// than a special case worked around.
fn placement_holds(want: Option<&Key>, got: Option<&EntityRow>) -> bool {
    let (Some(want), Some(got)) = (want, got) else {
        return false;
    };
    // Three fields, equal in all three. **Not** a near miss: two declarations of
    // one identity are two rows with the same path, kind and qualified name and
    // different ordinals, so comparing on the qualified name alone would let
    // `Pair<U>.first` pass for `Pair.first`. This is the same identity rule
    // `definitions` is scored on, and using one rule in both places is what stops
    // the two dimensions disagreeing about what a name means.
    got.key() == *want
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
    format!("{}|{}|{}|{}", path, local, module, alias.unwrap_or("-"))
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
                    let names: Vec<String> = pack.unit_ids().into_iter().map(render_id).collect();
                    failures.push(format!(
                        "peek({:?}) did not name {}; it named {names:?}",
                        context.question,
                        context.expected.render()
                    ));
                }
            }
            Err(error) => failures.push(format!("peek({:?}) failed: {error}", context.question)),
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
