//! The root module of a package nested inside another package, with no source root of its own.
//!
//! `nestedpkg/outer/` is a package because `nestedpkg/outer/src/lib.rs` exists, and this file makes
//! `nestedpkg/outer/inner` a package as well. **The nearest one wins**: a file under `inner` is in
//! `inner`, because taking the outermost package root would merge the two namespaces and make
//! every cross-crate path between them unspellable — which is the only thing this table is for.

mod b;
mod sub;

fn main() {
    let _ = b::Thing;
    let _ = sub::thing::Thing;
}