//! The `reexport` package: a crate root that publishes part of its own namespace.
//!
//! All three shapes are `Imports` edges. A re-export is still a binding every other file in the
//! module can be resolved against, and the resolver's first rung reads only `Imports`, so making
//! a re-export a differently-typed edge would remove a placement rather than add one. What
//! distinguishes them is in the relation's basis.

pub mod inner;

pub use crate::inner::Thing as Renamed;
pub use crate::inner::helpers::*;
