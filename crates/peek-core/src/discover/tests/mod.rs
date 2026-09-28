//! Tests for file discovery.
//!
//! Each submodule covers one policy area, and each test states the Cortex defect it prevents in its
//! name or its first comment. A test that cannot be traced to a defect is either documenting a new
//! guarantee — in which case the comment says what guarantee — or it does not belong.

mod classification;
mod exclusion_policy;
mod gitignore_rules;
mod identity;
mod limits;
mod ordering;
mod support;
mod symlinks;
mod walk;

use crate::discover::{Discovery, DiscoveryOptions, FileDiscovery};

use self::support::mixed_fixture;

/// Discover the committed fixture tree with the default options.
///
/// Panicking here rather than returning a `Result` is deliberate: a fixture that cannot be found
/// is a broken checkout, and a test that skipped would report success while asserting nothing.
pub fn discover_fixture() -> Discovery {
    discover_fixture_with(DiscoveryOptions::default())
}

/// Discover the committed fixture tree with the given options.
pub fn discover_fixture_with(options: DiscoveryOptions) -> Discovery {
    let root = mixed_fixture();
    FileDiscovery::new(&root, options)
        .discover()
        .expect("discover the committed fixture tree")
}
