//! A module file directly under the package directory, so there is no `src` in its path at all.

use crate::flags::HiArgs;

/// Builds a search over some haystack.
pub struct SearcherBuilder {
    args: Option<HiArgs>,
}

impl SearcherBuilder {
    pub fn new() -> SearcherBuilder {
        SearcherBuilder { args: None }
    }
}