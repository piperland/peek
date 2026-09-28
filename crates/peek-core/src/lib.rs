//! Peek core — the local codebase intelligence engine.
//!
//! This crate is the single source of truth for repository intelligence. The CLI and the MCP
//! server are thin adapters over this engine; neither contains domain logic of its own.
//!
//! The [`model`] module is the foundation everything else is written against.

/// Engine version, surfaced by `peek status` and the MCP `server_info` primitive.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod extract;
pub mod model;

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_semver_shaped() {
        let parts: Vec<&str> = super::VERSION.split('.').collect();
        assert!(parts.len() >= 2, "version must be major.minor[.patch]");
    }
}
