//! The Peek semantic model.
//!
//! These types are the contract every other subsystem is written against: storage, extraction,
//! resolution, query, and context compilation all read and write exactly these structures.
//!
//! Properties that are load-bearing, each defended by tests in this module:
//!
//! 1. **Identity does not depend on line numbers.** A symbol that moves down a file keeps its
//!    identity, so the relations pointing at it survive the move.
//! 2. **Uncertainty is a value, not an absence.** Every relation carries a typed
//!    [`relation::ResolutionState`]. An edge that could not be proven is persisted and
//!    queryable rather than being silently dropped or silently asserted.
//! 3. **Paths preserve case.** Repository-relative, `/`-separated, never lowercased.

pub mod entity;
pub mod language;
pub mod path;
pub mod relation;
pub mod span;

pub use entity::{Entity, EntityId, EntityKind};
pub use language::{CapabilityTier, Language};
pub use path::{PathError, RepoPath};
pub use relation::{
    ALL_RELATION_KINDS, Evidence, Relation, RelationKey, RelationKind, ResolutionState,
    UnresolvedReason, relation_kind_names,
};
pub use span::Span;
