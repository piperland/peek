//! Extraction: turning source text into entities and relations.
//!
//! Extraction is deliberately **table-driven and per language**. See [`spec`] for why, and
//! [`grammar`] for the validation that keeps a spec honest against the real Tree-sitter grammar.
//!
//! [`modules`] holds the file-to-module convention, which is also a table, and is the reason a
//! cross-crate `use` is a name the resolver can look up rather than a name it can only guess at.
//!
//! [`bindings`] holds the other half of that story: the node types that bind a name **the
//! index holds no entity for**, so a relation naming one is a use of a local and no entity
//! anywhere can be its referent.

pub mod bindings;
pub mod grammar;
pub mod modules;
pub mod registry;
pub mod source;
pub mod spec;
pub mod walker;

pub use spec::{LanguageSpec, ModuleLayout, NameStrategy};
pub use walker::{ExtractedFile, extract, extract_in_repo, extract_with, extract_with_roots};
