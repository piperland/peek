//! A crate with no source root: the modules sit directly under the package directory.
//!
//! This is ripgrep's shape. Every file here gets the package `core` because `main.rs` is in this
//! directory, so `flags/defs.rs` is `core::flags::defs` rather than a package of its own.

use crate::flags::HiArgs;
use crate::search::SearcherBuilder;

pub mod flags;
mod search;

fn main() {
    let args = flags::parse();
    let _searcher: SearcherBuilder = SearcherBuilder::new();
    let _: Option<HiArgs> = None;
}