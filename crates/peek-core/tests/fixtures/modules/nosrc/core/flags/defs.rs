//! Two levels below the crate root, in a crate with no `src` directory.
//!
//! `use crate::flags::Flag;` in `nosrc/core/main.rs` can only be a lookup if this file is reachable
//! under a name the import spells. Naming it after the directory above it — `flags` — gives the
//! package `flags` and the module `flags::defs`, and then `crate::flags::Flag` asks the table for
//! `flags::flags::Flag`, which is a spelling no source can produce.

/// Whether to match case-sensitively.
#[derive(Clone, Copy)]
pub struct Case;

/// One switch on the command line.
pub struct Flag;

/// Every switch the tool understands.
pub struct Flags;