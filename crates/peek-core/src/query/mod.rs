//! The query engine: traversal, explanation, and the context compiler.
//!
//! Everything through the resolver is built and served. This is the layer that turns an index
//! into answers, and the answers carry their own provenance: what a question was, which store
//! generation answered it, what each edge's resolution state was, and — for the context
//! compiler — what the token budget was, what it bought, and what it left out.
//!
//! # The three capabilities
//!
//! | | |
//! |---|---|
//! | [`explain`] | Why does this edge exist, and how sure is the engine? |
//! | [`dependents`] | Who depends on this, within a stated number of hops? |
//! | [`peek`] | A token-budgeted slice of the repository. |
//!
//! `callers` and `callees` are [`dependents`] with a different direction and a depth of one; see
//! the `traverse` module for why they are the same walk and what a caller can therefore expect
//! from all three.
//!
//! # How to call it
//!
//! [`Query`] is a borrowed façade over a [`Store`]. It holds no state of its own, so two queries
//! over the same store cannot disagree, and the options it carries are plain data that a CLI or an
//! MCP server can set from a request:
//!
//! ```no_run
//! # use peek_core::query::Query;
//! # use peek_core::store::Store;
//! # fn example(store: &Store) -> Result<(), Box<dyn std::error::Error>> {
//! let query = Query::new(store);
//! let pack = query.peek("PaymentService::retry", 8_000)?;
//! for unit in &pack.units {
//!     println!("{} — {}", unit.entity.summary(), unit.reason.describe());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The free functions [`explain`], [`explain_relation`], [`callers`], [`callees`],
//! [`dependents`] and [`peek`] are the same operations with the default options, for a caller
//! that has no reason to change them.
//!
//! # What this module will not do
//!
//! * **Invent intent.** There is no natural-language claim here that a relation does not support.
//!   Audit B11 found the predecessor's `explain()` emitting exactly that; the fix was structural,
//!   in the schema, and this module is the read side of it.
//! * **Pick between candidates.** D-0004. An ambiguous target is [`QueryError::AmbiguousTarget`]
//!   carrying every candidate, and an ambiguous edge in an answer keeps its candidate list.
//! * **Truncate quietly.** D-0009. The context compiler reports what it dropped, why, and at
//!   what cost, and a budget too small to hold that report is refused rather than exceeded.
//! * **Follow an unproven edge.** Nothing here traverses an `Ambiguous` or `Unresolved` relation.
//!   They are counted, reported and priced, never traversed.

mod context;
mod error;
mod explain;
mod tokens;
mod traverse;

#[cfg(test)]
mod tests;

pub use context::{
    BudgetReport, BudgetStatus, ContextEdge, ContextPack, ContextUnit, InclusionReason, Matched,
    Omission, OmissionReason, Omitted, structural_target,
};
pub use error::QueryError;
pub use explain::{ChainStep, EdgeSide, ExplainedEdge, Explanation, Subject};
pub use tokens::{CHARS_PER_TOKEN, Cost, TokenCounter};
pub use traverse::{Direction, Step, Walk, WalkRequest};

use serde::{Deserialize, Serialize};

use crate::model::entity::EntityId;
use crate::model::relation::Relation;
use crate::store::Store;

/// Relations read per visited entity, and the value handed to `Store::incoming` /
/// `Store::outgoing`.
///
/// **A cost bound, not a quality threshold.** No measurement says a repository's forty-seventh
/// call edge is less interesting than its first, and inventing such a number is exactly the
/// defect this project exists to remove. It bounds the work one adjacency seek may do, and a walk
/// that reaches it sets [`Walk::bounded`] and says so.
pub const DEFAULT_RELATIONS_PER_ENTITY: usize = 64;

/// Entities expanded by one structural walk.
///
/// The same reasoning: a bound on work, not a claim about importance. It exists because a walk
/// over a cyclic graph is only terminated by its own bookkeeping, and a second, coarse bound means
/// a bug in that bookkeeping cannot turn into an unbounded traversal.
pub const DEFAULT_MAX_VISITED: usize = 512;

/// Candidates the context compiler may rank.
///
/// Bounds the neighbourhood, which is otherwise the target's own degree and therefore already
/// bounded. Present so the cost of `peek` is expressible without first measuring the target.
pub const DEFAULT_MAX_UNITS: usize = 256;

/// Entities read from one file when a target names that file, and the value handed to
/// `Store::entities_in_file` and `Store::entities_with_qualified_name`.
///
/// Chosen to sit above the largest file measured in the real-repository probe
/// (`.agent/EVIDENCE/REAL-REPO-regex.md`: 227 Rust files, whose largest single file yields under
/// 100 entities) while still bounding a pathological one. A file above it produces a note and a
/// smaller answer rather than a silent one.
pub const DEFAULT_ENTITIES_PER_FILE: usize = 2_000;

/// Edges priced per included declaration.
///
/// Bounds one declaration's block, and the omissions it causes are named. Fifty is above the
/// median out-degree of a function in the same probe and below the tail, so the common case is not
/// shaped by this number.
pub const DEFAULT_MAX_EDGES_PER_UNIT: usize = 50;

/// Hops the provenance chain may walk in one [`explain`].
///
/// Four is enough to reach from a method to its file's callers without turning an explanation
/// into a traversal, and it is a parameter so a caller who wants a different answer can have one.
pub const DEFAULT_CHAIN_DEPTH: u32 = 4;

/// How a query is bounded, and what it believes.
///
/// Plain data with no hidden defaults, so a CLI or an MCP server can build it from a request and
/// an answer can report the exact policy that produced it. Every field is a *bound* or a
/// *policy*, never a ranking weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryOptions {
    /// Relations read per visited entity. See [`DEFAULT_RELATIONS_PER_ENTITY`].
    pub relations_per_entity: usize,
    /// Entities expanded by one walk. See [`DEFAULT_MAX_VISITED`].
    pub max_visited: usize,
    /// Candidates the compiler may rank. See [`DEFAULT_MAX_UNITS`].
    pub max_units: usize,
    /// Entities read from one file. See [`DEFAULT_ENTITIES_PER_FILE`].
    pub entities_per_file: usize,
    /// Edges priced per declaration. See [`DEFAULT_MAX_EDGES_PER_UNIT`].
    pub max_edges_per_unit: usize,
    /// Hops the provenance chain may walk. See [`DEFAULT_CHAIN_DEPTH`].
    pub chain_depth: u32,
    /// Whether a walk or a compiled pack follows `Inferred` edges. `true` by default, and every
    /// step or unit that arrived that way says so. See the `traverse` module for the reasoning.
    pub follow_inferred: bool,
    /// Whether an ambiguous edge in a pack names its candidates rather than only counting them.
    /// `true` by default: the count alone tells a reader that a choice exists but not what it is.
    pub list_ambiguity_candidates: bool,
    /// How tokens are counted. See [`TokenCounter`].
    pub counter: TokenCounter,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            relations_per_entity: DEFAULT_RELATIONS_PER_ENTITY,
            max_visited: DEFAULT_MAX_VISITED,
            max_units: DEFAULT_MAX_UNITS,
            entities_per_file: DEFAULT_ENTITIES_PER_FILE,
            max_edges_per_unit: DEFAULT_MAX_EDGES_PER_UNIT,
            chain_depth: DEFAULT_CHAIN_DEPTH,
            follow_inferred: true,
            list_ambiguity_candidates: true,
            counter: TokenCounter::default(),
        }
    }
}

/// A borrowed façade over a [`Store`].
///
/// Holds no state, so a query is a pure function of `(store, options, arguments)`. Two calls with
/// equal arguments over an unchanged index return equal values, which is the property the
/// determinism test asserts and the reason the type is worth having.
#[derive(Debug, Clone, Copy)]
pub struct Query<'a> {
    store: &'a Store,
    options: QueryOptions,
}

impl<'a> Query<'a> {
    /// A query with the default options.
    #[must_use]
    pub fn new(store: &'a Store) -> Self {
        Self {
            store,
            options: QueryOptions::default(),
        }
    }

    /// A query with explicit options.
    #[must_use]
    pub fn with_options(store: &'a Store, options: QueryOptions) -> Self {
        Self { store, options }
    }

    /// The store being read.
    #[must_use]
    pub fn store(&self) -> &'a Store {
        self.store
    }

    /// The options in force.
    #[must_use]
    pub fn options(&self) -> &QueryOptions {
        &self.options
    }

    /// Why does this entity exist, what does it do, and what does it point at?
    ///
    /// See the `explain` module documentation for the four things an answer contains and for why
    /// the chain is one path rather than all of them.
    pub fn explain(&self, id: &EntityId) -> Result<Explanation, QueryError> {
        explain::explain_entity(self, id)
    }

    /// Why does *this relation* exist?
    ///
    /// Takes the relation itself rather than a lookup key, because a caller standing on an edge
    /// already has it — it came out of [`Store::outgoing`] or [`Store::incoming`] — and inventing
    /// a second way to name an edge would be a second way to name it wrongly.
    pub fn explain_relation(&self, relation: &Relation) -> Result<Explanation, QueryError> {
        explain::explain_relation(self, relation)
    }

    /// Walk the graph from `target`.
    ///
    /// The one traversal; [`Query::callers`], [`Query::callees`] and [`Query::dependents`] are
    /// this with a different [`WalkRequest`].
    pub fn walk(&self, target: &EntityId, request: WalkRequest) -> Result<Walk, QueryError> {
        traverse::walk(self.store, &self.options, target, request)
    }

    /// One hop inbound: what uses this. See the `traverse` module for the definition of a caller.
    pub fn callers(&self, target: &EntityId) -> Result<Walk, QueryError> {
        self.walk(target, WalkRequest::callers())
    }

    /// One hop outbound: what this uses.
    pub fn callees(&self, target: &EntityId) -> Result<Walk, QueryError> {
        self.walk(target, WalkRequest::callees())
    }

    /// D-0007's `dependents`: reverse structural traversal, `depth` hops.
    ///
    /// `depth` is a parameter rather than a constant, and `0` returns nothing — the target is
    /// reported as [`Walk::target`] and never as its own dependent, because self-inclusion
    /// destroys the distance information that makes the answer worth having.
    pub fn dependents(&self, target: &EntityId, depth: u32) -> Result<Walk, QueryError> {
        self.walk(target, WalkRequest::dependents(depth))
    }

    /// Compile a token-budgeted slice of the repository.
    ///
    /// `budget_tokens` is a hard ceiling. The returned pack never costs more, reports what it
    /// dropped and why, and is refused outright when the budget cannot hold the report that would
    /// say so. See the `context` module documentation for the arithmetic.
    pub fn peek(&self, target: &str, budget_tokens: u64) -> Result<ContextPack, QueryError> {
        context::peek(self, target, budget_tokens)
    }

    /// The smallest budget [`Query::peek`] will accept.
    ///
    /// The cost of the report itself: the price of saying "nothing". Obtainable before asking, so
    /// a caller can reject a hopeless budget without paying for a failed compilation.
    #[must_use]
    pub fn minimum_budget(&self) -> u64 {
        context::report_reserve(self.options.counter)
    }
}

/// Why does this entity exist, and what is it connected to?
pub fn explain(store: &Store, id: &EntityId) -> Result<Explanation, QueryError> {
    Query::new(store).explain(id)
}

/// Why does this relation exist?
pub fn explain_relation(store: &Store, relation: &Relation) -> Result<Explanation, QueryError> {
    Query::new(store).explain_relation(relation)
}

/// One hop inbound from `target`.
pub fn callers(store: &Store, target: &EntityId) -> Result<Walk, QueryError> {
    Query::new(store).callers(target)
}

/// One hop outbound from `target`.
pub fn callees(store: &Store, target: &EntityId) -> Result<Walk, QueryError> {
    Query::new(store).callees(target)
}

/// Reverse structural traversal, `depth` hops. D-0007.
pub fn dependents(store: &Store, target: &EntityId, depth: u32) -> Result<Walk, QueryError> {
    Query::new(store).dependents(target, depth)
}

/// A token-budgeted slice of the repository, centred on `target`.
pub fn peek(store: &Store, target: &str, budget_tokens: u64) -> Result<ContextPack, QueryError> {
    Query::new(store).peek(target, budget_tokens)
}
