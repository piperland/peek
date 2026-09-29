//! `peek explain`, `peek callers`, `peek callees` and `peek dependents`.
//!
//! # How a target string becomes an entity
//!
//! **Through the context compiler, deliberately.** The engine's resolver tries three lookups in a
//! fixed order — a repository path, a qualified name, then a bare name with `File` entities held
//! back — and reports which one matched. It is not public: `Query::explain` takes an `EntityId` and
//! `Query::peek` takes a string, so the string-to-entity step is reachable only from inside the
//! crate.
//!
//! The alternatives were both worse. Reimplementing the three lookups here would duplicate the
//! resolver's *rules*, and the failure mode of duplicated rules is that they drift: `peek explain`
//! would resolve a string one way and `peek context` another, and a caller would have no way to
//! tell which answer to believe. That is D-0004's failure in a new place.
//!
//! So this module calls [`peek_core::query::Query::peek`] at the smallest budget the engine
//! accepts, which resolves the target and returns a pack whose units are all omissions, and takes
//! the identity off that pack. Two lines, no duplicated rule, and the two commands are guaranteed
//! to agree about what a string names because they asked the same function. The cost is one
//! neighbourhood enumeration per command, which is bounded by the target's own degree and is
//! negligible beside the walk these commands perform anyway.
//!
//! # `callers`, `callees` and `dependents` are one walk
//!
//! D-0007. There is no second algorithm here and there is not meant to be: the three differ only
//! in direction and depth, and the answer records which of the three names the caller used.

use peek_core::model::EntityId;
use peek_core::query::{Explanation, Matched, Query, QueryError, Walk, WalkRequest};

use crate::answer::{Answer, ExplainAnswer, WalkAnswer};
use crate::commands::{Outcome, need};
use crate::exit::{Failure, Refusal, kind};
use crate::paths::{self, Location};

/// A target string resolved to exactly one entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The identity.
    pub id: EntityId,
    /// Which of the engine's three lookups matched.
    pub matched: Matched,
}

/// Resolve a target string, through the engine's own resolver.
///
/// See the module documentation for why this is not three lookups written out again.
pub fn resolve(
    store: &peek_core::store::Store,
    text: &str,
    command: &'static str,
) -> Result<Resolved, Failure> {
    let query = Query::new(store);
    // The smallest budget that can hold the engine's own report. Compilation then has no room for
    // content, so the pack comes back empty and refused — which is the correct answer here, because
    // the units are not what was asked for; the target identity is.
    let minimum = query.minimum_budget();
    match query.peek(text, minimum) {
        Ok(pack) => Ok(Resolved {
            id: pack.target.id().clone(),
            matched: pack.target.matched,
        }),
        Err(error) => Err(refusal_from_query(command, error)),
    }
}

/// Turn a query failure into a refusal, keeping the candidates and the minimum.
///
/// D-0004: an ambiguity is a result the caller resolves, so every candidate travels with the
/// refusal rather than being counted. D-0009: a budget too small to hold the report is a refusal
/// with the number attached, not an error string.
pub fn refusal_from_query(command: &'static str, error: QueryError) -> Failure {
    match error {
        QueryError::UnknownTarget { query, detail } => Failure::refused(
            command,
            Refusal::new(
                kind::UNKNOWN_TARGET,
                format!("no indexed entity matches {query:?}: {detail}"),
            ),
        ),
        QueryError::AmbiguousTarget {
            query,
            matched,
            candidates,
        } => Failure::refused(
            command,
            Refusal::new(
                kind::AMBIGUOUS_TARGET,
                format!(
                    "{query:?} is {matched} for {} indexed entities; this build will not choose \
                     between them, so name one",
                    candidates.len()
                ),
            )
            .with_candidates(candidates.iter().map(EntityId::display).collect()),
        ),
        QueryError::NotIndexed { query } => Failure::refused(
            command,
            Refusal::new(
                kind::UNKNOWN_TARGET,
                format!(
                    "no entity is indexed as {query}; the string resolved to an identity nothing \
                     holds, which usually means the index moved under this command"
                ),
            ),
        ),
        QueryError::BudgetTooSmall { requested, minimum } => Failure::refused(
            command,
            Refusal::new(
                kind::BUDGET_TOO_SMALL,
                format!(
                    "a budget of {requested} token(s) cannot hold the {minimum}-token report that \
                     would state what was left out, so no answer was produced"
                ),
            )
            .with_minimum(minimum),
        ),
        QueryError::Store(error) => Failure::failed(
            command,
            Refusal::new(kind::ENGINE, format!("the index could not be read: {error}")),
        ),
    }
}

/// Open the store, refusing rather than creating an index.
///
/// Every read command shares this, so "a query against a repository that was never indexed refuses
/// instead of inventing an empty answer" is one rule in one place.
pub fn open_for_query(
    location: Option<&Location>,
    command: &'static str,
) -> Result<peek_core::store::Store, Failure> {
    let location = need(location, command)?;
    if !location.index_existed {
        return Err(Failure::refused(
            command,
            Refusal::new(
                kind::NO_INDEX,
                format!(
                    "there is no index for this repository: {} does not exist, so there is \
                     nothing to ask. `peek index` builds one",
                    location.index_text()
                ),
            ),
        ));
    }
    paths::open_store(location, command)
}

/// `peek explain <TARGET>`.
pub fn explain(
    location: Option<&Location>,
    command: &'static str,
    text: &str,
) -> Result<Outcome, Failure> {
    let store = open_for_query(location, command)?;
    let resolved = resolve(&store, text, command)?;
    let query = Query::new(&store);
    let explanation = query
        .explain(&resolved.id)
        .map_err(|error| refusal_from_query(command, error))?;
    let uncertain_edges = explanation.uncertain().count() as u64;
    let answer = ExplainAnswer {
        target: text.to_owned(),
        matched: resolved.matched.as_str().to_owned(),
        uncertain_edges,
        explanation,
        did_not: vec![
            "this explained one declaration and did not walk to the things it calls or is called \
             by. `peek callers` and `peek callees` are that walk"
                .to_owned(),
            "the chain is one chosen edge per hop, not every path here; the `alternatives` count on \
             each step is how many edges were passed over, and `chosen_because` states the rule \
             that picked the one that was taken"
                .to_owned(),
            "this did not search for a name close to the one given. The index matches a name, a \
             qualified name or a path exactly, and holds nothing to rank near-misses by, so a \
             refusal here is the whole of what is known"
                .to_owned(),
        ],
    };
    Ok(Outcome::ok(Answer::Explain(answer)))
}

/// `peek callers`, `peek callees` and `peek dependents`, which are one walk with three names.
pub fn walk(
    location: Option<&Location>,
    command: &'static str,
    text: &str,
    request: WalkRequest,
) -> Result<Outcome, Failure> {
    let store = open_for_query(location, command)?;
    let resolved = resolve(&store, text, command)?;
    let query = Query::new(&store);
    let walked: Walk = query
        .walk(&resolved.id, request)
        .map_err(|error| refusal_from_query(command, error))?;
    let inferred_steps = walked.inferred().count() as u64;

    let mut did_not = vec![
        "an ambiguous or unresolved edge is read and counted but never followed, because \
         following an edge with no proven target would mean picking one. That is why the relation \
         count is larger than the step count"
            .to_owned(),
        "structural edges — defines, contains, owns — are the seed of this walk, not a hop in it; \
         they say where a declaration lives rather than what depends on what"
            .to_owned(),
    ];
    if walked.bounded {
        did_not.push(format!(
            "a limit stopped the walk before it finished, so this answer may be short: {} \
             entit(ies) were expanded of at most {}",
            walked.visited, walked.options.max_visited
        ));
    }
    if walked.seeds.is_empty() {
        did_not.push(
            "the target is a declaration rather than a container, so the walk started from it alone \
             rather than from what it contains"
                .to_owned(),
        );
    } else {
        did_not.push(format!(
            "the target is a container, so the walk started from its {} member(s) at distance 1: \
             this answers \"what depends on anything in it\", and one more hop would walk outward \
             from each member",
            walked.seeds.len()
        ));
    }
    if walked.request.depth == 0 {
        did_not.push(
            "a depth of 0 returns nothing by design: the target is named in the answer and is never \
             its own dependent, because self-inclusion destroys the distance information that makes \
             the answer worth having (D-0007)"
                .to_owned(),
        );
    }

    let answer = WalkAnswer {
        command: command.to_owned(),
        target: text.to_owned(),
        matched: resolved.matched.as_str().to_owned(),
        inferred_steps,
        walk: walked,
        did_not,
    };
    Ok(Outcome::ok(Answer::Walk(answer)))
}

/// Whether every edge in an explanation carries a resolution state.
///
/// An edge with an empty state would be a row the index could not explain about itself, and D-0003
/// put the field in the schema for exactly that. The engine's own tests assert it; this exists so a
/// caller of the CLI can assert it on *this crate's* answer without reaching into the engine's
/// test module, and so the check has a name rather than being an inline `all()`.
#[must_use]
pub fn every_edge_is_explained(explanation: &Explanation) -> bool {
    explanation.edges.iter().all(|edge| !edge.state.is_empty())
}
