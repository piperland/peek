//! `explain`: why does this edge exist, and how confident is the answer?
//!
//! # The capability Cortex advertised and could not deliver
//!
//! Audit B11: Cortex's `explain()` counted inbound edges with no kind filter and labelled the
//! result "N callers", which was off by at least one *always* because every symbol is guaranteed a
//! `Defines` edge. It emitted prose about intent that nothing in the data supported. And audit
//! B10: `Edge.reason` was a constant string on every producer, so a call resolved by a globally
//! unique name and one resolved by `symbols.first()` were byte-identical records. There was no
//! field anywhere that could distinguish them, and the function therefore had nothing to be honest
//! *with*.
//!
//! D-0003 put the field in the schema from the first commit: every relation carries a typed
//! [`ResolutionState`], and `Pending` and `Inferred` carry a `basis` string written by whoever
//! made the decision. This module is the read side of that decision. It reports what is recorded
//! and nothing else — there is no sentence in this file that a relation does not support, because a
//! list of edges and a list of chain hops are the entire vocabulary.
//!
//! # The four things an answer contains
//!
//! 1. **The resolution state, as the typed value it is.** [`ExplainedEdge::relation`] carries the
//!    whole [`Relation`], so the state in the output is the state in the index rather than a
//!    re-encoding that could drift from it.
//! 2. **The evidence class**, verbatim from [`ResolutionState::evidence_class`].
//! 3. **The `basis` string the resolver wrote** — or `None`, honestly.
//!    [`ResolutionState::Resolved`] has no `basis` field, by design: for a proof the evidence class
//!    *is* the whole argument, and the resolver writes a basis only when the decision was an
//!    inference that needs its claim spelled out. Synthesising a sentence here would recreate the
//!    Cortex defect exactly — an `Edge.reason` that says nothing while appearing to say something.
//! 4. **The span**, as recorded.
//!
//! Plus the candidates of an ambiguous edge, and the chain.
//!
//! # The chain, and why it is one path rather than all of them
//!
//! "The chain of edges that led here" is a *chain*: one provenance path from the subject back
//! towards the start of the question, not a second copy of the whole inbound graph. [`Walk`]
//! already answers "what reaches this, within N hops" and is the right tool for that question; a
//! second traversal here would be the same walk under a second name, which is what D-0007 was
//! recorded to stop.
//!
//! So the chain asks, at each hop, "which single edge best explains this arrival?" — and then says
//! how many it passed over. [`ChainStep::alternatives`] is that count and
//! [`ChainStep::chosen_because`] states the rule that selected the edge. The choice is not hidden,
//! it is *disclosed*:
//!
//! * **Strongest evidence first**, because the chain is the argument for why the subject is
//!   connected at all, and the strongest edge is the one the engine would stand behind. Strength
//!   is [`Evidence::strength`], the same ordering the model already uses to rank competing
//!   candidates.
//! * **Then the store's own identity order**, a total order over the natural key. Two runs over an
//!   unchanged index therefore choose the same edge, and a test says so.
//!
//! The chain follows containment as well as dependency, because the first hop of a provenance path
//! for a method is "this is a member of that", which is a better explanation than any dependency
//! edge the method has. It obeys the same [`QueryOptions::follow_inferred`] policy as a walk. It
//! terminates on a cycle: a node already on the chain stops it, and says so in a note rather than
//! looping.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::entity::{Entity, EntityId};
use crate::model::relation::{Relation, RelationKey, ResolutionState};
use crate::query::error::QueryError;
use crate::query::{Query, QueryOptions};

/// Which side of the subject an edge is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeSide {
    /// The subject does this.
    Outgoing,
    /// This happens to the subject.
    Incoming,
}

impl EdgeSide {
    /// A stable, lowercase label for CLI and MCP output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            EdgeSide::Outgoing => "outgoing",
            EdgeSide::Incoming => "incoming",
        }
    }

    /// The arrow a rendered line uses.
    #[must_use]
    pub const fn arrow(self) -> &'static str {
        match self {
            EdgeSide::Outgoing => "->",
            EdgeSide::Incoming => "<-",
        }
    }
}

/// What is being explained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Subject {
    /// A declaration: its record, the edges leaving it, and the edges arriving at it.
    Entity(Entity),
    /// A single relation — usually one read out of [`crate::store::Store::outgoing`] or
    /// [`crate::store::Store::incoming`], which is how a caller comes to be standing on an edge.
    ///
    /// Boxed because the two variants differ in size enough that an unboxed enum would be laid out
    /// around the larger of them on every construction.
    Relation(Box<Relation>),
}

impl Subject {
    /// The entity at the centre of the explanation: the declaration itself, or the one the
    /// relation originates from.
    #[must_use]
    pub fn entity(&self) -> &EntityId {
        match self {
            Subject::Entity(entity) => &entity.id,
            Subject::Relation(relation) => &relation.source,
        }
    }

    /// One line naming the subject, for a report that needs a heading.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Subject::Entity(entity) => entity.summary(),
            Subject::Relation(relation) => format!(
                "{} {} `{}` at {}:{}",
                relation.source.qualified_name(),
                relation.kind,
                relation.target_name,
                relation.source.path(),
                relation.span.start_line
            ),
        }
    }
}

/// One edge, with everything the index recorded about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedEdge {
    /// Whether the subject is the source or the target.
    pub side: EdgeSide,
    /// The relation exactly as stored, including its typed [`ResolutionState`].
    pub relation: Relation,
    /// [`ResolutionState::describe`], verbatim. For an ambiguous edge this reads
    /// `ambiguous (3 candidates)`: the count is part of the state, so a consumer reading only the
    /// rendered line still learns the edge is undecided and how undecided it is.
    pub state: String,
    /// The evidence class, when the state has one. `None` for `Ambiguous` and `Unresolved`,
    /// because there is no evidence for a target that was never established.
    pub evidence_class: Option<String>,
    /// The `basis` string the extractor or resolver wrote, verbatim — or `None` because the state
    /// carries none. See the module documentation for why `None` is the honest answer for a
    /// `Resolved` edge rather than a synthesised sentence.
    pub basis: Option<String>,
}

impl ExplainedEdge {
    /// The candidates this edge was ambiguous between; empty for any other state.
    ///
    /// One source of truth: the list lives in [`Relation::resolution`], so the count printed by
    /// `state` and the list read through this method cannot disagree.
    #[must_use]
    pub fn candidates(&self) -> &[EntityId] {
        match &self.relation.resolution {
            ResolutionState::Ambiguous { candidates } => candidates,
            _ => &[],
        }
    }

    /// How many candidates an ambiguous edge was ambiguous between.
    #[must_use]
    pub fn candidate_count(&self) -> usize {
        self.candidates().len()
    }

    /// One line a person or an agent can read, carrying the state and the evidence.
    #[must_use]
    pub fn render(&self) -> String {
        let place = match self.relation.target.as_ref() {
            Some(target) => target.display(),
            None => self.relation.target_name.clone(),
        };
        let mut line = format!(
            "{} {} {}  [{}]  {}:{}",
            self.side.arrow(),
            self.relation.kind,
            place,
            self.state,
            self.relation.source.path(),
            self.relation.span.start_line
        );
        if let Some(basis) = &self.basis {
            line.push_str(&format!("\n     because: {basis}"));
        }
        if self.relation.resolution.is_ambiguous() {
            let names: Vec<String> = self.candidates().iter().map(EntityId::display).collect();
            line.push_str(&format!(
                "\n     between: {}",
                if names.is_empty() {
                    "no candidate was recorded".to_owned()
                } else {
                    names.join(", ")
                }
            ));
        }
        line
    }
}

/// One hop of the provenance chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainStep {
    /// The entity this hop arrived at.
    pub id: EntityId,
    /// Hops from the subject. Always at least 1.
    pub distance: u32,
    /// The edge that led here, in full.
    pub via: Relation,
    /// How many followable edges reached the *previous* node. The chain took one of them; this is
    /// how many it passed over, which is the difference between "the only way in" and "one way
    /// in, chosen by a stated rule".
    pub alternatives: usize,
    /// The rule that chose this edge, in one clause.
    pub chosen_because: String,
}

impl ChainStep {
    /// Whether this hop rests on an inference rather than a proof.
    #[must_use]
    pub fn is_inferred(&self) -> bool {
        matches!(self.via.resolution, ResolutionState::Inferred { .. })
    }

    /// One line a person or an agent can read.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{} {} via {} ({}) — {}",
            self.distance,
            self.id.display(),
            self.via.kind,
            self.via.resolution.describe(),
            self.chosen_because
        )
    }
}

/// A complete answer to "why does this exist".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explanation {
    /// What was explained.
    pub subject: Subject,
    /// The edges at the subject: the ones leaving it, then the ones arriving, each sorted by its
    /// natural key so the order is a property of the data rather than of the query plan.
    pub edges: Vec<ExplainedEdge>,
    /// The provenance path, one chosen edge per hop, ending at the subject.
    pub chain: Vec<ChainStep>,
    /// The depth the chain was allowed, whether or not it used it.
    pub chain_depth: u32,
    /// The options in force, so the answer says what it was allowed to read.
    pub options: QueryOptions,
    /// Statements the answer owes the reader. Each is a claim the engine can back with what it
    /// read: a limit that was hit, a chain that ended, an ambiguity whose candidates were missing
    /// from the relation and had to be re-read.
    pub notes: Vec<String>,
}

impl Explanation {
    /// The entity at the centre of the explanation.
    #[must_use]
    pub fn entity(&self) -> &EntityId {
        self.subject.entity()
    }

    /// The edges leaving the subject.
    pub fn outgoing(&self) -> impl Iterator<Item = &ExplainedEdge> {
        self.edges
            .iter()
            .filter(|edge| edge.side == EdgeSide::Outgoing)
    }

    /// The edges arriving at the subject.
    pub fn incoming(&self) -> impl Iterator<Item = &ExplainedEdge> {
        self.edges
            .iter()
            .filter(|edge| edge.side == EdgeSide::Incoming)
    }

    /// The edges whose target could not be proven, of either failure mode.
    ///
    /// A caller asking "what does the engine *not* know about this" wants exactly these, and
    /// D-0009's point is that they are as much a part of the answer as the certain ones.
    pub fn uncertain(&self) -> impl Iterator<Item = &ExplainedEdge> {
        self.edges.iter().filter(|edge| {
            edge.relation.resolution.is_ambiguous() || edge.relation.resolution.is_unresolved()
        })
    }

    /// The whole answer as text, in the order it is meant to be read.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = self.subject.describe();
        for note in &self.notes {
            text.push_str(&format!("\nnote: {note}"));
        }
        for edge in &self.edges {
            text.push_str(&format!("\n  {}", edge.render()));
        }
        if self.chain.is_empty() {
            text.push_str("\nchain: nothing in the index points at this");
        } else {
            text.push_str("\nchain:");
            for step in &self.chain {
                text.push_str(&format!("\n  {}", step.render()));
            }
        }
        text
    }
}

/// Explain every edge at an entity, and the chain that leads to it.
pub(crate) fn explain_entity(query: &Query<'_>, id: &EntityId) -> Result<Explanation, QueryError> {
    let Some(entity) = query.store().entity(id)? else {
        return Err(QueryError::NotIndexed {
            query: id.display(),
        });
    };

    let outgoing = query
        .store()
        .outgoing(id, None, query.options().relations_per_entity)?;
    let incoming = query
        .store()
        .incoming(id, None, query.options().relations_per_entity)?;

    let mut notes: Vec<String> = Vec::new();
    if outgoing.len() >= query.options().relations_per_entity {
        notes.push(format!(
            "only the first {} outgoing relations were read; there may be more",
            query.options().relations_per_entity
        ));
    }
    if incoming.len() >= query.options().relations_per_entity {
        notes.push(format!(
            "only the first {} incoming relations were read; there may be more",
            query.options().relations_per_entity
        ));
    }
    if incoming.is_empty() {
        notes.push(format!(
            "nothing in the index points at {}; as far as the index knows it is a root",
            id.display()
        ));
    }

    let mut edges: Vec<ExplainedEdge> = Vec::with_capacity(outgoing.len() + incoming.len());
    for relation in outgoing {
        edges.push(convert(EdgeSide::Outgoing, relation, &mut notes, query)?);
    }
    for relation in incoming {
        edges.push(convert(EdgeSide::Incoming, relation, &mut notes, query)?);
    }
    sort_edges(&mut edges);

    let (chain, chain_notes) = build_chain(query, id, query.options().chain_depth)?;
    notes.extend(chain_notes);

    Ok(Explanation {
        subject: Subject::Entity(entity),
        edges,
        chain,
        chain_depth: query.options().chain_depth,
        options: *query.options(),
        notes,
    })
}

/// Explain one relation, and the chain that leads to the declaration containing it.
pub(crate) fn explain_relation(
    query: &Query<'_>,
    relation: &Relation,
) -> Result<Explanation, QueryError> {
    let mut notes: Vec<String> = Vec::new();
    let mut edges: Vec<ExplainedEdge> = vec![convert(
        EdgeSide::Outgoing,
        relation.clone(),
        &mut notes,
        query,
    )?];

    // The edges arriving at the relation's source are the second half of the answer. They are
    // the same fact a chain walks, but the chain picks one and these are all of them, so a caller
    // can see the alternatives without having to re-derive them.
    if query.store().entity(&relation.source)?.is_some() {
        let incoming =
            query
                .store()
                .incoming(&relation.source, None, query.options().relations_per_entity)?;
        if incoming.len() >= query.options().relations_per_entity {
            notes.push(format!(
                "only the first {} relations of {} were read; there may be more",
                query.options().relations_per_entity,
                relation.source.display()
            ));
        }
        for other in incoming {
            if other.natural_key() == relation.natural_key() {
                continue;
            }
            edges.push(convert(EdgeSide::Incoming, other, &mut notes, query)?);
        }
    } else {
        notes.push(format!(
            "{} is not in the index, so nothing can be said about what points at it",
            relation.source.display()
        ));
    }
    sort_edges(&mut edges);

    let (chain, chain_notes) = build_chain(query, &relation.source, query.options().chain_depth)?;
    notes.extend(chain_notes);

    Ok(Explanation {
        subject: Subject::Relation(Box::new(relation.clone())),
        edges,
        chain,
        chain_depth: query.options().chain_depth,
        options: *query.options(),
        notes,
    })
}

/// A total order on edges: side first, then the natural key.
///
/// The natural key is the same twelve fields the store's `UNIQUE` constraint uses, so the order of
/// the output is a property of the data rather than of whichever query plan happened to run.
fn sort_edges(edges: &mut [ExplainedEdge]) {
    edges.sort_by(|a, b| {
        a.side
            .cmp(&b.side)
            .then_with(|| a.relation.natural_key().cmp(&b.relation.natural_key()))
    });
}

/// Turn a relation into the reported form, re-reading a missing candidate list if necessary.
fn convert(
    side: EdgeSide,
    mut relation: Relation,
    notes: &mut Vec<String>,
    query: &Query<'_>,
) -> Result<ExplainedEdge, QueryError> {
    if relation.resolution.is_ambiguous() {
        let recorded = match &relation.resolution {
            ResolutionState::Ambiguous { candidates } => candidates.len(),
            _ => 0,
        };
        if recorded == 0 {
            // An ambiguous relation read back from the store always carries its candidates; one
            // that does not is a real, reportable state — the resolver found more than one match
            // and then failed to record them. Re-reading the rows is honest, because the rows are
            // the authority; reporting "ambiguous (0 candidates)" would not be.
            let from_store = query.store().ambiguous_candidates(
                &relation.source,
                relation.kind,
                &relation.target_name,
            )?;
            if from_store.is_empty() {
                notes.push(format!(
                    "`{}` is ambiguous but no candidate row exists for it; the resolver found \
                     more than one match and did not record them",
                    relation.target_name
                ));
            } else {
                let recovered = from_store.len();
                relation.resolution = ResolutionState::Ambiguous {
                    candidates: from_store,
                };
                notes.push(format!(
                    "the relation read back with an empty candidate list; {recovered} were \
                     re-read from `relation_candidate`"
                ));
            }
        }
    }
    let state = relation.resolution.describe();
    let evidence_class = relation.resolution.evidence_class().map(str::to_owned);
    let basis = basis_of(&relation.resolution);
    Ok(ExplainedEdge {
        side,
        relation,
        state,
        evidence_class,
        basis,
    })
}

/// The `basis` a state carries, or `None` because it carries none.
///
/// `Resolved` returns `None` on purpose. The resolver writes a basis for `Pending` and `Inferred`
/// because those are claims that need their reasoning spelled out; for a proof the evidence class
/// is the whole argument. A synthesised sentence here would be an `Edge.reason` that says nothing
/// — the exact defect D-0003 was recorded to remove.
fn basis_of(resolution: &ResolutionState) -> Option<String> {
    match resolution {
        ResolutionState::Pending { basis, .. } | ResolutionState::Inferred { basis, .. } => {
            Some(basis.clone())
        }
        ResolutionState::Resolved { .. }
        | ResolutionState::Ambiguous { .. }
        | ResolutionState::Unresolved { .. } => None,
    }
}

/// Walk backwards from `start`, one chosen edge per hop, up to `depth` hops.
fn build_chain(
    query: &Query<'_>,
    start: &EntityId,
    depth: u32,
) -> Result<(Vec<ChainStep>, Vec<String>), QueryError> {
    let options = *query.options();
    let mut chain: Vec<ChainStep> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut on_chain: BTreeSet<EntityId> = BTreeSet::new();
    on_chain.insert(start.clone());
    let mut node = start.clone();

    for distance in 1..=depth {
        let read = query
            .store()
            .incoming(&node, None, options.relations_per_entity)?;
        if read.len() >= options.relations_per_entity {
            notes.push(format!(
                "the chain read only the first {} relations of {}; it may have missed a stronger \
                 edge",
                options.relations_per_entity,
                node.display()
            ));
        }
        let candidates: Vec<Relation> = read
            .iter()
            .filter(|edge| chainable(edge, &options))
            .cloned()
            .collect();
        let Some(via) = pick(&candidates) else {
            notes.push(format!(
                "nothing in the index points at {}; the chain ends here",
                node.display()
            ));
            break;
        };
        let next = via.source.clone();
        if !on_chain.insert(next.clone()) {
            notes.push(format!(
                "the chain returns to {}, which it has already visited; it stops rather than \
                 looping",
                next.display()
            ));
            break;
        }
        chain.push(ChainStep {
            id: next.clone(),
            distance,
            via: via.clone(),
            alternatives: candidates.len(),
            chosen_because: why_chosen(via, candidates.len()),
        });
        node = next;
        if distance == depth {
            notes.push(format!(
                "the chain stopped at the requested depth of {distance} hop(s); it may continue \
                 further"
            ));
        }
    }
    Ok((chain, notes))
}

/// Whether an edge may carry the chain.
///
/// Containment counts, unlike in a walk: the first hop of a provenance path for a method is "this
/// is a member of that", which explains more than any dependency edge the method has. Ambiguous
/// and unresolved edges cannot, because there is no target to walk back from.
fn chainable(edge: &Relation, options: &QueryOptions) -> bool {
    if !edge.is_followable() {
        return false;
    }
    options.follow_inferred || !matches!(edge.resolution, ResolutionState::Inferred { .. })
}

/// The edge the chain takes: strongest evidence first, then the natural key.
///
/// A total order, so the choice is reproducible; the edge that wins is the one the engine would
/// defend, and the number that lost is reported in [`ChainStep::alternatives`].
fn pick(candidates: &[Relation]) -> Option<&Relation> {
    let mut best: Option<&Relation> = None;
    for candidate in candidates {
        best = match best {
            None => Some(candidate),
            Some(current) => {
                if rank(candidate) < rank(current) {
                    Some(candidate)
                } else {
                    Some(current)
                }
            }
        };
    }
    best
}

/// The sort key, ascending: reversed strength so the strongest evidence sorts first.
fn rank(relation: &Relation) -> (Reverse<u16>, RelationKey) {
    (
        Reverse(strength_of(&relation.resolution)),
        relation.natural_key(),
    )
}

/// The evidence strength behind a state, or zero when there is none because there is no target.
fn strength_of(resolution: &ResolutionState) -> u16 {
    match resolution {
        ResolutionState::Pending { evidence, .. }
        | ResolutionState::Resolved { by: evidence }
        | ResolutionState::Inferred { by: evidence, .. } => u16::from(evidence.strength()),
        ResolutionState::Ambiguous { .. } | ResolutionState::Unresolved { .. } => 0,
    }
}

/// The sentence explaining which edge was taken and why, printed in the answer.
fn why_chosen(chosen: &Relation, alternatives: usize) -> String {
    match alternatives {
        0 | 1 => "it is the only edge that reaches this entity".to_owned(),
        _ => format!(
            "its evidence ({}) is the strongest of {alternatives} edges reaching this entity",
            chosen
                .resolution
                .evidence_class()
                .unwrap_or("no evidence class")
        ),
    }
}
