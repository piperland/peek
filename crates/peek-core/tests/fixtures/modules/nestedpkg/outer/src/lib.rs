//! A package with a source root, which also holds a nested package beneath it.
//!
//! `nestedpkg/outer/src/lib.rs` makes `nestedpkg/outer` a package. `nestedpkg/outer/inner/` holds
//! a `main.rs` of its own, which makes `inner` a *separate* package nested inside the first. A file
//! under `inner` belongs to `inner` and not to `outer`, and the tests say so.

pub mod a;

pub use a::Thing;