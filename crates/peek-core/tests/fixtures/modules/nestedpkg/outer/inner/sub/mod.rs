//! A module directory inside the nested package, two levels below its root.
//!
//! `nestedpkg/outer` is a package because it has a `src/lib.rs`, and `nestedpkg/outer/inner` is a
//! package because it has a `main.rs`. **The nearest one wins**, so this is the module
//! `inner::sub` and not `outer::sub`. Taking the outermost would merge the two namespaces and make
//! every cross-crate path between them unspellable, which is the only thing this table is for.

mod thing;