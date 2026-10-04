//! A directory that is a module and a package at once.
//!
//! `nosrc/core/flags/mod.rs` names the module `flags`, and `nosrc/core/main.rs` — two levels up —
//! is the crate root, so the whole tree is one package called `core`. The file declares no
//! package: only the file that *is* a package's root module does that.

mod complete;
mod defs;

pub use defs::{Case, Flag, Flags};

/// The high-level arguments, as the crate root asks for them.
pub struct HiArgs;

/// Parse the command line.
pub fn parse() -> Option<HiArgs> {
    None
}