//! The gate fixture crate root.
//!
//! Deliberately small and completely readable. Every declaration in this tree is
//! listed in `gate.expect`, and a reviewer should be able to check that list
//! against this file by eye without running anything.
//!
//! Two names here are declared twice on purpose — `format_line` in `report`
//! and again in `service` — because a graph that resolves by bare name picks the
//! wrong one and a graph that does not is measurably better.

pub mod model;
pub mod report;
pub mod service;
pub mod traits;

pub use model::Entry;
pub use traits::{Clock, Saveable};

/// The maximum number of rows a single report may contain.
pub const MAX_ROWS: usize = 512;

/// How many times a retry is attempted before it is given up on.
pub static RETRY_LIMIT: u32 = 3;

/// A named threshold, declared as an alias so the alias itself is a symbol.
pub type Threshold = u32;

/// The entry point. Calls one function in each module, so the callee set spans
/// files rather than sitting inside a single body.
pub fn summarise(entries: &[Entry]) -> String {
    let kept: Vec<Entry> = entries.iter().take(MAX_ROWS).cloned().collect();
    let mut out = format_line("head", kept.len());
    for entry in &kept {
        out.push_str(&service::describe(entry));
    }
    report::render(&kept, &mut out)
}

/// A macro, so the gate measures macro invocation as a call form.
#[macro_export]
macro_rules! counted {
    ($count:expr, $body:expr) => {{
        let mut n = 0usize;
        let result = $body;
        n += 1;
        (result, n)
    }};
}

/// Counts `body`, for a caller that needs a tally as well as a value.
pub fn counted(body: impl FnOnce() -> usize) -> (usize, usize) {
    counted!(body())
}

/// Declares a nested module inline, so the ownership chain is `outer.inner.fn`.
pub mod outer {
    /// The middle module.
    pub mod inner {
        /// The innermost function.
        pub fn deep() -> u8 {
            super::super::summarise(&[]).len() as u8
        }
    }
}