//! Extraction: turning source text into entities and relations.
//!
//! Extraction is deliberately **table-driven and per language**. See [`spec`] for why, and
//! [`grammar`] for the validation that keeps a spec honest against the real Tree-sitter grammar.

pub mod grammar;
pub mod registry;
pub mod spec;

pub use spec::{LanguageSpec, NameStrategy};
