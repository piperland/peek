//! The query engine's failure taxonomy.
//!
//! A query that cannot be answered is not the same as a query that is empty, and collapsing the
//! two is how a tool ends up asserting something it did not check. Every variant here is a
//! statement the engine can back with what it actually found:
//!
//! * [`QueryError::UnknownTarget`] — nothing in the index answers to that string, and the message
//!   says so rather than returning an empty answer that reads like "this symbol has nothing to do
//!   with anything".
//! * [`QueryError::AmbiguousTarget`] — several entities answer to it, and every one of them is
//!   carried in the error. D-0004: the caller disambiguates, never this module.
//! * [`QueryError::BudgetTooSmall`] — the budget cannot even hold the report that would explain
//!   what was left out. D-0009: the answer is a refusal, not a truncated slice.

use thiserror::Error;

use crate::model::entity::EntityId;
use crate::query::context::Matched;
use crate::store::StoreError;

/// Everything that can go wrong while answering a question about the index.
#[derive(Debug, Error)]
pub enum QueryError {
    /// The store could not be read. Carried through unchanged so the storage taxonomy survives
    /// into the query surface instead of being flattened into one string.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// No indexed entity answers to `query`.
    ///
    /// `detail` says what was tried in the order it was tried, so a caller who mistyped a path
    /// learns that the path was the shape they got wrong rather than guessing.
    #[error("no indexed entity matches {query:?}: {detail}")]
    UnknownTarget { query: String, detail: String },

    /// Several entities answer to `query`, and none of them is more correct than the rest.
    ///
    /// D-0004 bans a silent pick. The candidates are returned strongest-first, and the caller
    /// disambiguates; picking the first would make this error impossible to write, which is the
    /// point.
    #[error("{query:?} is {matched} for {} indexed entities; name one of them", candidates.len())]
    AmbiguousTarget {
        query: String,
        matched: Matched,
        candidates: Vec<EntityId>,
    },

    /// An [`EntityId`] names something the index does not hold.
    ///
    /// A distinct variant from [`QueryError::UnknownTarget`] because they are different failures.
    /// `UnknownTarget` is a *string* that matches nothing; this is an identity the caller already
    /// holds — read out of a previous answer, or built by hand — and the store has no row for it.
    /// The second usually means the index moved under the caller, which is worth telling apart
    /// from a typo.
    #[error("no entity is indexed as {query}")]
    NotIndexed { query: String },

    /// The token budget is smaller than the report that would state what was dropped.
    ///
    /// Returned rather than exceeded. A pack whose own explanation does not fit is a pack whose
    /// silence is the answer, which is the defect D-0009 exists to prevent. `minimum` is the
    /// cost of the empty report, obtainable ahead of time from
    /// [`crate::query::Query::minimum_budget`].
    #[error(
        "a budget of {requested} tokens cannot hold the {minimum}-token report; raise the budget or ask a narrower question"
    )]
    BudgetTooSmall { requested: u64, minimum: u64 },
}
