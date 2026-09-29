//! Turning a target string into one indexed identity.
//!
//! # Why the compiler and not a resolver of this crate's own
//!
//! The engine resolves a target with three lookups in a fixed order — a repository path, a
//! qualified name, then a bare name with `File` entities held back — and reports which one
//! matched. Those rules belong to the engine and the engine is where they are tested. Writing them
//! out again here would be a second copy of a resolution order, and the failure mode of a
//! duplicated order is that it drifts: `explain` would name a symbol one way and `context` another,
//! and nothing at either end would say which to believe.
//!
//! So this module does not resolve anything. It asks [`Query::peek`] for the **smallest budget the
//! engine accepts** and takes the identity off the pack that comes back. At that budget the pack
//! has no room for content, so the units come back empty and the target is the only thing in it —
//! which is what the caller asked for. The resolution that happened is the engine's own, on the
//! same call `context` makes.
//!
//! # Why the command line does the same thing
//!
//! `crates/peek-cli/src/commands/query.rs` resolves a target this way and its module comment says
//! that a second route into the resolver was considered and not taken. That is the reason there is
//! no `Query::resolve` in the engine and the reason this exists instead: one route, used by both
//! surfaces, with the engine's public surface no wider than it was before either of them existed.
//!
//! The cost is one neighbourhood enumeration per call, which is bounded by the target's own degree
//! and is negligible beside the walk or the explanation the caller is about to ask for anyway.

use peek_core::model::EntityId;
use peek_core::query::{Query, QueryError, QueryOptions};
use peek_core::store::Store;

/// The one indexed entity `target` names.
///
/// Returns the identity and nothing else. Which of the three lookups matched travels in the
/// compiler's own pack, which `context` returns whole, and in the refusal for an ambiguity, which
/// is built from the same `QueryError`; neither `explain` nor a walk has a field for it, so
/// inventing one here would be a second place for it to go stale.
///
/// `options` is the caller's own, rather than the default, so a lookup runs under the same limits
/// the answer it is about to produce will be built with.
pub fn resolve(
    store: &Store,
    options: QueryOptions,
    target: &str,
) -> Result<EntityId, QueryError> {
    let query = Query::with_options(store, options);
    // The smallest budget that can hold the engine's own report. Compilation then has no room for
    // content, so the pack comes back with nothing in it but the target.
    let minimum = query.minimum_budget();
    let pack = query.peek(target, minimum)?;
    Ok(pack.target.id().clone())
}
