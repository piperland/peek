//! Structural traversal: callers, callees, and the D-0007 `dependents` boundary.
//!
//! # One walk, three names
//!
//! `callers`, `callees` and `dependents` are the same operation with different parameters, and
//! saying so is the point rather than an accident. D-0007 established that Cortex's `impact()`
//! was reverse structural traversal under a name that promised patch-risk analysis it never did,
//! and audit B12 *proves* it: every relation emits a typed edge plus an identical-endpoint
//! `DependsOn` twin, so `impact(t, d) ≡ dependencies(t, Inbound, d)`. There is no second
//! algorithm hiding behind the first name here, and adding one would recreate the confusion the
//! decision removed.
//!
//! What differs between the three is written down rather than implied:
//!
//! | Name | Direction | Depth | Kinds |
//! |---|---|---|---|
//! | [`WalkRequest::callers`] | inbound | 1 | every dependency kind |
//! | [`WalkRequest::callees`] | outbound | 1 | every dependency kind |
//! | D-0007 `dependents` | inbound | *a parameter* | every dependency kind |
//!
//! A caller is defined as *anything with a followable edge into the target*, not as "something
//! that issues a `calls` edge". A macro that names a symbol, or a type that mentions one, is as
//! real a dependency as an invocation, and narrowing the definition would make the traversal
//! quietly incomplete. A caller who wants only invocations asks for
//! `kind: Some(RelationKind::Calls)`, which is a filter rather than a different meaning.
//!
//! # What a walk follows
//!
//! **Only edges with exactly one proven target.** A relation is followed when
//! [`Relation::is_followable`] holds — that is, when its [`ResolutionState`] is `Resolved` or
//! `Inferred` *and* it names a target entity. `Ambiguous` and `Unresolved` edges are read and
//! counted, and are never followed: following an edge with no target means picking one, which is
//! D-0004.
//!
//! **`Inferred` edges are followed, and the caller can always tell.** This is a deliberate
//! decision and the reasoning is worth stating, because the alternative is a plausible-looking
//! default. An `Inferred` edge has exactly one target and an evidence class that supports it; it
//! differs from `Resolved` by carrying a *basis* string, which the resolver writes precisely so a
//! consumer can audit the claim. Hiding inferred edges would therefore discard recorded, auditable
//! information on the grounds that it is weaker — and would do it silently, producing a graph
//! that looks complete and is not. A caller who wants only proofs sets
//! [`QueryOptions::follow_inferred`] to `false`; either way [`Step::via`] carries the whole
//! relation, so `matches!(step.via.resolution, ResolutionState::Inferred { .. })` is the test and
//! [`Walk::followed_inferred`] is the count.
//!
//! **Structural edges are not followed.** `defines`, `contains` and `owns` are
//! [`RelationKind::is_structural`] and say where a declaration lives rather than what depends on
//! what. They are the *seed* of a walk on a structural target, not a hop in one. They are still
//! read by [`crate::query::explain`], which does want them.
//!
//! # Why a file answers a different question from a symbol
//!
//! A symbol's dependents are the things that name it. A file's are not, because almost nothing
//! names a file: a module that `mod`s it produces one edge, and every function inside it is
//! depended upon by its own callers. So a walk whose target is a `File`, `Module`, `Package`,
//! `Workspace` or `Repository` **expands to its members first**, at distance 1, with the
//! containment edge that proves membership as the edge that led there. `dependents(ledger.rs, 1)`
//! is therefore "who depends on anything in `ledger.rs`", which is the question a person means
//! when they type it — and `dependents(settle, 1)` is "who calls `settle`". Depth 2 from the file
//! then walks outward from each member, exactly as it would from a symbol.
//!
//! # Where a file's members come from, and why that edge is inferred
//!
//! `EntityKind::File` and `RelationKind::Defines` both exist, and `defines` is the natural edge
//! from a file to what it declares — but **the extractor does not emit it today.** It emits
//! `Contains` from an enclosing *declaration* to a nested one, so a top-level function has no
//! containment edge at all and a file has none to its own declarations. Reading
//! [`membership`] therefore has two halves: the containment edges the index really holds, and
//! `Store::entities_in_file` for a file, whose rows the primary key proves belong to it.
//!
//! The second half becomes an `Inferred` edge rather than a `Resolved` one, and that is the whole
//! point of the type. The fact is true and it is *established* — an entity row's key begins with
//! its file path, so `entities_in_file` is a range scan of the primary key and cannot return a
//! declaration belonging to another file — but it is a fact derived here from one the index
//! recorded, not one it recorded. `Resolved` would present a derived edge as a stored one.
//! `Inferred` carries a `basis` naming the derivation, the walk follows it by default, and every
//! step that arrived that way says so: `matches!(step.via.resolution, ResolutionState::Inferred
//! { .. })`. A caller who wants only stored edges sets
//! [`QueryOptions::follow_inferred`] to `false` and gets the narrower answer.
//!
//! When the extractor does emit `defines`, the first half finds the real edge and the second
//! never runs. Nothing else in this module changes.
//!
//! # Termination
//!
//! Termination is by construction, not by a counter that might be miscounted. The frontier is a
//! binary heap ordered by `(distance, identity)`, and an entity is enqueued once and expanded once,
//! so the walk's work is bounded by [`QueryOptions::max_visited`] whatever the graph does. First
//! discovery is therefore at the shortest distance, and the target is never re-entered — D-0007
//! requires that `dependents` not include its own target, because self-inclusion destroys the
//! distance information that makes the answer useful.
//!
//! Two counters describe the edges the walk *declined*, and they are the answer to "did this
//! terminate because of a cycle or in spite of one". [`Walk::closed`] counts every followable edge
//! whose other end the walk had already reached — the target included — and [`Walk::revisits`]
//! counts the subset that reached an entity already found rather than the target. A `closed` of
//! zero means the neighbourhood is a tree and no bookkeeping was needed.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use serde::{Deserialize, Serialize};

use crate::model::entity::{Entity, EntityId, EntityKind};
use crate::model::relation::{Evidence, Relation, RelationKind, ResolutionState};
use crate::model::span::Span;
use crate::query::QueryOptions;
use crate::query::error::QueryError;
use crate::store::Store;

/// Which way a walk goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Follow edges arriving at the target: its callers, its dependents.
    Inbound,
    /// Follow edges leaving the target: its callees.
    Outbound,
}

impl Direction {
    /// A stable, lowercase label for CLI and MCP output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Direction::Inbound => "inbound",
            Direction::Outbound => "outbound",
        }
    }

    /// The phrase a person-facing report uses for the relationship.
    #[must_use]
    pub const fn phrase(self) -> &'static str {
        match self {
            Direction::Inbound => "is used by",
            Direction::Outbound => "uses",
        }
    }
}

/// What to walk, and how far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkRequest {
    pub direction: Direction,
    /// Hops to walk. `0` returns nothing, because the target is reported in [`Walk::target`]
    /// rather than as its own dependent — D-0007, again.
    pub depth: u32,
    /// One relation kind, or every kind. `None` is the default because a walk is a question
    /// about dependencies as a whole.
    pub kind: Option<RelationKind>,
}

impl WalkRequest {
    /// One hop inbound: who uses this.
    #[must_use]
    pub const fn callers() -> Self {
        Self {
            direction: Direction::Inbound,
            depth: 1,
            kind: None,
        }
    }

    /// One hop outbound: what this uses.
    #[must_use]
    pub const fn callees() -> Self {
        Self {
            direction: Direction::Outbound,
            depth: 1,
            kind: None,
        }
    }

    /// `depth` hops inbound: D-0007's `dependents`, with the depth a parameter rather than a
    /// constant the caller cannot see.
    #[must_use]
    pub const fn dependents(depth: u32) -> Self {
        Self {
            direction: Direction::Inbound,
            depth,
            kind: None,
        }
    }

    /// Restrict the walk to one relation kind.
    #[must_use]
    pub const fn with_kind(mut self, kind: Option<RelationKind>) -> Self {
        self.kind = kind;
        self
    }
}

/// One entity the walk reached, and the edge that led there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The entity reached.
    pub id: EntityId,
    /// Hops from the target. Always at least 1: the target is never its own step.
    pub distance: u32,
    /// The edge that led here, in full — kind, span, evidence and resolution state.
    ///
    /// Carried whole rather than projected to an identity so that nothing a consumer is shown
    /// about this edge had to be thrown away in order to walk the graph. A caller can always ask
    /// how certain this arrival was, because the answer is right here.
    pub via: Relation,
}

impl Step {
    /// Whether this arrival rests on an inference rather than a proof.
    ///
    /// The whole of [`QueryOptions::follow_inferred`], seen from the consumer's side.
    #[must_use]
    pub fn is_inferred(&self) -> bool {
        matches!(self.via.resolution, ResolutionState::Inferred { .. })
    }

    /// The resolution state of the edge that led here, in the model's own vocabulary.
    #[must_use]
    pub fn state(&self) -> String {
        self.via.resolution.describe()
    }
}

/// The result of one traversal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Walk {
    /// What was asked about. Present in every walk including an empty one, so an empty result is
    /// distinguishable from a target that was never resolved.
    pub target: EntityId,
    pub request: WalkRequest,
    /// The options in force, so the answer says what it was allowed to do.
    pub options: QueryOptions,
    /// The entities reached, ordered by `(distance, identity)`. A total order, so two runs over
    /// an unchanged index produce identical output.
    pub steps: Vec<Step>,
    /// The members a structural target expanded to before the walk began. Empty for a symbol.
    pub seeds: Vec<EntityId>,
    /// Entities whose edges were read. Each is enqueued once and expanded once, which is what
    /// terminates the walk.
    pub visited: usize,
    /// Relations read across every expanded entity, of every resolution state. Includes the ones
    /// that were not followed, because "we read it and could not follow it" is information.
    pub inspected: usize,
    /// How many steps arrived through an `Inferred` edge. Never more than `steps.len()`, and
    /// always equal to the number of steps [`Walk::inferred`] yields, so a caller can check the
    /// count against the list rather than trust it.
    pub followed_inferred: usize,
    /// Followable edges the walk declined to follow because it had already reached the entity at
    /// the other end, or because that entity is the target.
    ///
    /// Zero means the neighbourhood around the target is a tree and no bookkeeping was needed.
    /// Non-zero means the graph is not, and is how a test tells "terminated *because* of a cycle"
    /// from "failed to hang".
    pub closed: usize,
    /// Of [`Walk::closed`], the ones that reached an entity the walk had *already found* rather
    /// than the target. So `closed - revisits` is the number of edges that turned back to the
    /// target itself, which is what a cycle looks like from inside a reverse walk.
    pub revisits: usize,
    /// A limit stopped the walk before it finished, so this answer may be incomplete.
    ///
    /// The depth limit does **not** set it: asking for one hop and getting one hop is the
    /// question that was asked.
    pub bounded: bool,
}

impl Walk {
    /// Whether the walk finished everything its bounds allowed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.bounded
    }

    /// The steps at one distance, in order.
    pub fn at_distance(&self, distance: u32) -> impl Iterator<Item = &Step> {
        self.steps
            .iter()
            .filter(move |step| step.distance == distance)
    }

    /// The greatest distance reached, or `None` for an empty walk.
    #[must_use]
    pub fn max_distance(&self) -> Option<u32> {
        self.steps.iter().map(|step| step.distance).max()
    }

    /// Whether `id` is among the results.
    #[must_use]
    pub fn contains(&self, id: &EntityId) -> bool {
        self.steps.iter().any(|step| &step.id == id)
    }

    /// The steps that arrived by inference.
    pub fn inferred(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|step| step.is_inferred())
    }

    /// One line naming the relationship, for a CLI or MCP surface to print above the list.
    #[must_use]
    pub fn headline(&self) -> String {
        let via = match self.request.kind {
            Some(kind) => format!("via `{kind}` edges"),
            None => "via any dependency edge".to_owned(),
        };
        let furthest = match self.max_distance() {
            Some(distance) => format!("{distance} hop(s)"),
            None => "nothing reached".to_owned(),
        };
        format!(
            "{} {} {}; {} relation(s) read, {} step(s) found, furthest {furthest}",
            self.target.display(),
            self.request.direction.phrase(),
            via,
            self.inspected,
            self.steps.len()
        )
    }
}

/// Run one traversal.
pub(crate) fn walk(
    store: &Store,
    options: &QueryOptions,
    target: &EntityId,
    request: WalkRequest,
) -> Result<Walk, QueryError> {
    let seeds = members(store, options, target)?;

    let mut steps: BTreeMap<EntityId, Step> = BTreeMap::new();
    let mut frontier: BinaryHeap<Reverse<(u32, EntityId)>> = BinaryHeap::new();
    let mut enqueued: BTreeSet<EntityId> = BTreeSet::new();

    for seed in &seeds {
        if !steps.contains_key(&seed.id) {
            steps.insert(
                seed.id.clone(),
                Step {
                    id: seed.id.clone(),
                    distance: 1,
                    via: seed.via.clone(),
                },
            );
        }
        enqueue(&mut frontier, &mut enqueued, 1, &seed.id);
    }
    enqueue(&mut frontier, &mut enqueued, 0, target);

    let mut result = Walk {
        target: target.clone(),
        request,
        options: *options,
        steps: Vec::new(),
        seeds: seeds.iter().map(|seed| seed.id.clone()).collect(),
        visited: 0,
        inspected: 0,
        followed_inferred: 0,
        closed: 0,
        revisits: 0,
        bounded: false,
    };

    while let Some(Reverse((distance, node))) = frontier.pop() {
        // A node *at* the depth limit is reached but not expanded: its own neighbours are
        // further away than the caller asked for. That is the requested depth rather than a
        // bound, so `bounded` stays false.
        if distance >= request.depth {
            break;
        }
        if result.visited >= options.max_visited {
            result.bounded = true;
            break;
        }
        result.visited += 1;

        let edges = match request.direction {
            Direction::Inbound => {
                store.incoming(&node, request.kind, options.relations_per_entity)?
            }
            Direction::Outbound => {
                store.outgoing(&node, request.kind, options.relations_per_entity)?
            }
        };
        if edges.len() >= options.relations_per_entity {
            // The store stopped at its own `LIMIT`. There may be more edges than were read, and
            // the only honest thing is to say the walk may be short.
            result.bounded = true;
        }

        for edge in edges {
            result.inspected += 1;
            if !followable(&edge, options) {
                continue;
            }
            let Some(neighbour) = neighbour_of(&edge, request.direction) else {
                continue;
            };
            if &neighbour == target {
                // The target is never one of its own results: D-0007, because self-inclusion
                // destroys the distance information that makes the answer useful. Meeting it again
                // is a cycle closing.
                result.closed += 1;
                continue;
            }
            if steps.contains_key(&neighbour) {
                // A second path to something already found. Counted, because "this entity depends
                // on that one two ways" is a fact about the code, and because it is how a test
                // tells a cyclic neighbourhood from a tree.
                result.closed += 1;
                result.revisits += 1;
                continue;
            }
            // Counted where the step is made, so `followed_inferred` is exactly the number of
            // results that arrived by inference and never more — a bound a test can check rather
            // than a tally of how many inference edges happened to be read.
            if matches!(edge.resolution, ResolutionState::Inferred { .. }) {
                result.followed_inferred += 1;
            }
            steps.insert(
                neighbour.clone(),
                Step {
                    id: neighbour.clone(),
                    distance: distance + 1,
                    via: edge.clone(),
                },
            );
            enqueue(&mut frontier, &mut enqueued, distance + 1, &neighbour);
        }
    }

    let mut ordered: Vec<Step> = steps.into_values().collect();
    ordered.sort_by(|a, b| a.distance.cmp(&b.distance).then_with(|| a.id.cmp(&b.id)));
    result.steps = ordered;
    Ok(result)
}

/// A member of a structural target, and the containment edge that proves it.
struct Seed {
    id: EntityId,
    via: Relation,
}

/// Put a node on the frontier once, so the walk cannot expand it twice.
fn enqueue(
    frontier: &mut BinaryHeap<Reverse<(u32, EntityId)>>,
    enqueued: &mut BTreeSet<EntityId>,
    distance: u32,
    id: &EntityId,
) {
    if enqueued.insert(id.clone()) {
        frontier.push(Reverse((distance, id.clone())));
    }
}

/// The other end of an edge, in the direction being walked.
fn neighbour_of(edge: &Relation, direction: Direction) -> Option<EntityId> {
    match direction {
        Direction::Inbound => Some(edge.source.clone()),
        Direction::Outbound => edge.target.clone(),
    }
}

/// Whether an edge is one this walk will follow.
///
/// The three conditions, and what each one excludes:
///
/// * `kind.is_dependency()` — `defines`, `contains` and `owns` describe where a declaration
///   lives, not what depends on what.
/// * `is_followable()` — exactly one proven target, so following it is not a guess.
/// * the `follow_inferred` policy — an `Inferred` target is real and auditable, so it is followed
///   by default and every step says so.
fn followable(edge: &Relation, options: &QueryOptions) -> bool {
    if !edge.kind.is_dependency() || !edge.is_followable() {
        return false;
    }
    options.follow_inferred || !matches!(edge.resolution, ResolutionState::Inferred { .. })
}

/// The entities a structural target contains, each with the edge that proves membership.
///
/// Empty for a symbol: a function is not a container of its callers, and treating it as one
/// would put every caller in the seed set and turn a depth-1 walk into a depth-2 one.
fn members(
    store: &Store,
    options: &QueryOptions,
    target: &EntityId,
) -> Result<Vec<Seed>, QueryError> {
    if !structural_target(target.kind()) {
        return Ok(Vec::new());
    }
    let mut found: BTreeMap<EntityId, Relation> = BTreeMap::new();
    for edge in membership(store, options, target)? {
        let Some(member) = edge.target.as_ref() else {
            continue;
        };
        if member == target {
            continue;
        }
        found.entry(member.clone()).or_insert_with(|| edge.clone());
    }
    Ok(found
        .into_iter()
        .map(|(id, via)| Seed { id, via })
        .collect())
}

/// The edges that show what a container entity contains.
///
/// Two halves, and the module documentation explains why:
///
/// * the followable `defines` / `contains` / `owns` edges the index actually holds, which point
///   *out of* the container; and
/// * for a `File`, one `Inferred` `defines` edge per declaration in
///   [`Store::entities_in_file`], because the extractor records no file-to-declaration edge.
///
/// A stored edge always wins over a derived one for the same member, so a future extractor that
/// emits `defines` changes the answer from `inferred` to `resolved` and nothing else.
pub(crate) fn membership(
    store: &Store,
    options: &QueryOptions,
    container: &EntityId,
) -> Result<Vec<Relation>, QueryError> {
    let mut edges: Vec<Relation> = Vec::new();
    let mut recorded: BTreeSet<EntityId> = BTreeSet::new();
    for edge in store.outgoing(container, None, options.relations_per_entity)? {
        if !edge.kind.is_structural() || !edge.is_followable() {
            continue;
        }
        if let Some(child) = edge.target.as_ref() {
            recorded.insert(child.clone());
        }
        edges.push(edge);
    }
    if container.kind() == EntityKind::File {
        for entity in store.entities_in_file(container.path(), options.entities_per_file)? {
            if entity.id == *container || recorded.contains(&entity.id) {
                continue;
            }
            edges.push(inferred_membership(container, &entity));
        }
    }
    Ok(edges)
}

/// The `defines` edge between a file and a declaration it holds, for a member the index does not
/// record one for.
///
/// The evidence is [`Evidence::Containment`] because that is what it is: the declaration *is* in
/// that file, and the entity table's primary key begins with the file path, so
/// `Store::entities_in_file` is a range scan that cannot return a declaration from anywhere else.
/// The basis says where the fact came from, because an `Inferred` edge whose basis does not say so
/// is a confident guess — the thing this project exists to stop being.
fn inferred_membership(file: &EntityId, declared: &Entity) -> Relation {
    // A `File` entity has no span of its own, and a declaration always does; the zero-width span
    // at the origin is the honest fallback rather than a position invented from the file size.
    let span = declared.span.unwrap_or(Span {
        start_byte: 0,
        end_byte: 0,
        start_line: 1,
        start_column: 1,
        end_line: 1,
        end_column: 1,
    });
    Relation::inferred(
        RelationKind::Defines,
        file.clone(),
        declared.id.clone(),
        declared.name.clone(),
        span,
        Evidence::Containment,
        format!(
            "`{}` is declared in `{}`; the index records no `defines` edge, so membership was \
             read from the entity table rather than from a relation",
            declared.id.qualified_name(),
            file.path()
        ),
    )
}

/// Whether a target of this kind answers "what is inside it" rather than "what does it do".
///
/// A `File` is included because a file is exactly that: a container of declarations. `Module`,
/// `Package`, `Workspace` and `Repository` are the same question asked at a coarser level, and
/// they are here so that the answer is defined for them today rather than invented when the
/// extractor starts emitting them (R-007).
#[must_use]
pub fn structural_target(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::File
            | EntityKind::Module
            | EntityKind::Package
            | EntityKind::Workspace
            | EntityKind::Repository
    )
}
