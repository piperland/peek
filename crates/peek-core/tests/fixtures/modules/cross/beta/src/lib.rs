//! The `beta` package, which imports from `alpha`.
//!
//! This is the shape that dominated the measured decide rate on `rust-lang/regex`: the reference
//! names something in a *different* package, so without a module table there is nowhere to look
//! it up and the edge lands in the unresolved bucket as `no_candidate`.

pub mod service;

pub use alpha::gateway::Gateway;
