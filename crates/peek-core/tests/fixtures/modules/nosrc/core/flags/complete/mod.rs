//! Three levels below the crate root, and a module directory in its own right.
//!
//! The nearest package root above this file is `nosrc/core`, four levels up, so this is the module
//! `core::flags::complete` — not a package called `complete` reached from `core::flags`.

mod bash;
mod fish;

/// The shell completions this build offers.
pub fn names() -> Vec<&'static str> {
    vec![bash::NAME, fish::NAME]
}