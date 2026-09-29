//! A crate with three sibling module files and one inline module.
//!
//! Every `mod x;` here points at a file that also exists, so the tree has both halves of the
//! distinction: the *declaration* lives in this file and the *definition* lives in the child. The
//! two are different entities at different paths, and the module table needs both.

mod a;
mod b;
mod c;

pub use a::Alpha;

pub fn one() -> a::Alpha {
    a::Alpha
}
