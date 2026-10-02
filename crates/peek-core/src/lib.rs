//! Peek core — the local codebase intelligence engine.
//!
//! This crate is the single source of truth for repository intelligence. The CLI and the MCP
//! server are thin adapters over this engine; neither contains domain logic of its own.
//!
//! The [`model`] module is the foundation everything else is written against.

// Production code may not unwrap or expect. Both are denied by the workspace lint set because
// outside a test they hide a real failure behind a panic. Tests are allowed to use them: a test
// that cannot state its own precondition directly is harder to read than one that can.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

/// Engine version, surfaced by `peek status` and the MCP `server_info` primitive.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod containment;
pub mod discover;
pub mod doctor;
pub mod extract;
pub mod indexer;
pub mod model;
pub mod query;
pub mod resolve;
pub mod store;
pub mod watch;

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_semver_shaped() {
        let parts: Vec<&str> = super::VERSION.split('.').collect();
        assert!(parts.len() >= 2, "version must be major.minor[.patch]");
    }
}
