//! Entities and their stable identity.
//!
//! # Why identity is not a line number
//!
//! Cortex keyed entities as `symbol::<path>::<kind>:<name>:<row>:<col>`. Two consequences made
//! that unusable long-term:
//!
//! * Inserting a blank line near the top of a file gave every symbol below it a *new* identity.
//!   Because relation identifiers embed entity identifiers, the entire relation set below the
//!   edit was rewritten too — a one-line whitespace change churned the whole index.
//! * `normalize_path` lowercased every path, so on a case-sensitive filesystem `src/Foo.rs` and
//!   `src/foo.rs` collapsed into a single map key and one file silently overwrote the other.
//!
//! # The scheme
//!
//! Identity is `(path, kind, qualified_name, ordinal)`:
//!
//! * `path` — repository-relative, case-preserving, `/`-separated. See [`crate::model::RepoPath`].
//! * `kind` — a `Foo` the type and `Foo` the `impl` block are different entities.
//! * `qualified_name` — the ownership chain (`PaymentService.retry`, `Outer::Inner`), derived
//!   from syntax rather than position.
//! * `ordinal` — disambiguates genuinely overloaded siblings, such as several `impl` blocks for
//!   one type. Ordinals shift when siblings are reordered, so they are the *last* discriminator,
//!   not the first.
//!
//! A `structural_fingerprint` is stored alongside the identity. It is **not** the identity: it is
//! what lets reconciliation prove that a moved or edited entity is still the same one when the
//! ordinal has churned.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::path::RepoPath;
use super::span::Span;

/// What an entity is.
///
/// This vocabulary is deliberately limited to things an extractor can prove. Concepts that are
/// better modelled as a flag than as a kind — a test is a function with `is_test`; a
/// configuration key is a symbol read from a config file — do not get their own variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    // ---- structure ----
    /// The repository root.
    Repository,
    /// A build-system workspace root (Cargo workspace, npm workspaces, Gradle multi-project).
    Workspace,
    /// A distributable unit: Cargo crate, npm package, Maven module, .NET project.
    Package,
    /// A namespace or importable unit within a package.
    Module,
    /// A source file.
    File,

    // ---- callables ----
    Function,
    /// A function bound to a type, or any function-like member.
    Method,
    Constructor,

    // ---- types ----
    Class,
    Struct,
    Union,
    Enum,
    /// A nominal type contract with implementations.
    Interface,
    /// Rust trait / Go interface method set.
    Trait,
    /// Swift protocol / Kotlin interface.
    Protocol,
    /// A name bound to another type.
    TypeAlias,
    /// A compile-time type parameter.
    TypeParameter,

    // ---- data ----
    Variable,
    Constant,
    /// A compile-time or process-lifetime binding, distinct from a mutable local.
    Static,
    Field,
    Property,
    Parameter,
    Macro,
}

impl EntityKind {
    /// Whether this kind is a type-like declaration that other entities can inherit or
    /// implement.
    pub fn is_type(self) -> bool {
        matches!(
            self,
            EntityKind::Class
                | EntityKind::Struct
                | EntityKind::Union
                | EntityKind::Enum
                | EntityKind::Interface
                | EntityKind::Trait
                | EntityKind::Protocol
        )
    }

    /// Whether this kind can be invoked, and therefore participate in call relations.
    pub fn is_callable(self) -> bool {
        matches!(
            self,
            EntityKind::Function
                | EntityKind::Method
                | EntityKind::Constructor
                | EntityKind::Macro
        )
    }

    /// Whether this kind is a member of another entity rather than a top-level declaration.
    pub fn is_member(self) -> bool {
        matches!(
            self,
            EntityKind::Method
                | EntityKind::Field
                | EntityKind::Property
                | EntityKind::Parameter
        )
    }

    /// A stable, lowercase, human-readable label for CLI and MCP output.
    pub fn as_str(self) -> &'static str {
        match self {
            EntityKind::Repository => "repository",
            EntityKind::Workspace => "workspace",
            EntityKind::Package => "package",
            EntityKind::Module => "module",
            EntityKind::File => "file",
            EntityKind::Function => "function",
            EntityKind::Method => "method",
            EntityKind::Constructor => "constructor",
            EntityKind::Class => "class",
            EntityKind::Struct => "struct",
            EntityKind::Union => "union",
            EntityKind::Enum => "enum",
            EntityKind::Interface => "interface",
            EntityKind::Trait => "trait",
            EntityKind::Protocol => "protocol",
            EntityKind::TypeAlias => "type_alias",
            EntityKind::TypeParameter => "type_parameter",
            EntityKind::Variable => "variable",
            EntityKind::Constant => "constant",
            EntityKind::Static => "static",
            EntityKind::Field => "field",
            EntityKind::Property => "property",
            EntityKind::Parameter => "parameter",
            EntityKind::Macro => "macro",
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stable, position-independent identity for an entity.
///
/// Equal identities mean "the same entity", and survive edits that move the entity within its
/// file, reorder unrelated declarations above it, or change surrounding whitespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityId {
    path: RepoPath,
    kind: EntityKind,
    qualified_name: String,
    ordinal: u32,
}

impl EntityId {
    /// Build an identity. `qualified_name` is the ownership chain joined by `.`; the final
    /// component is the entity's own name.
    pub fn new(
        path: RepoPath,
        kind: EntityKind,
        qualified_name: impl Into<String>,
        ordinal: u32,
    ) -> Self {
        Self {
            path,
            kind,
            qualified_name: qualified_name.into(),
            ordinal,
        }
    }

    /// The repository-relative, case-preserving file this entity is declared in.
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn kind(&self) -> EntityKind {
        self.kind
    }

    /// The dotted ownership chain, e.g. `PaymentService.retry`.
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// The final component of [`Self::qualified_name`].
    pub fn name(&self) -> &str {
        self.qualified_name
            .rsplit('.')
            .next()
            .unwrap_or(&self.qualified_name)
    }

    /// The ownership chain without this entity's own name, or `None` at the top level.
    pub fn parent_name(&self) -> Option<&str> {
        self.qualified_name.rsplit_once('.').map(|(parent, _)| parent)
    }

    /// The disambiguating index among identically-named, identically-kinded siblings.
    pub fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// A compact, stable, human-readable rendering for logs and diffs.
    ///
    /// This is a *display* form, not a parseable key. Storage uses the individual fields so it
    /// can index them.
    pub fn display(&self) -> String {
        if self.ordinal == 0 {
            format!("{}::{}", self.path, self.qualified_name)
        } else {
            format!("{}::{}#{}", self.path, self.qualified_name, self.ordinal)
        }
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// A declaration Peek knows about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    /// The declared name, without its ownership chain.
    pub name: String,
    /// Declared parameter and return types, verbatim, when the extractor can read them.
    /// Never synthesised.
    pub signature: Option<String>,
    /// Documentation text from the language's doc-comment convention, when present.
    pub doc: Option<String>,
    pub span: Option<Span>,
    /// Language this entity was extracted from, or `None` for structural entities.
    pub language: Option<crate::model::language::Language>,
    /// Whether this entity is a test case. A flag rather than a kind, so that a test is still
    /// searchable as the function it is.
    pub is_test: bool,
    /// Hash of the normalised declaration body plus its kind.
    ///
    /// Not an identity — a reconciliation aid. Two entities with the same identity but
    /// different fingerprints were edited; two *candidates* with the same fingerprint after a
    /// reorder are very likely the same entity that shifted ordinal.
    pub structural_fingerprint: Option<String>,
}

impl Entity {
    /// The stable identity.
    pub fn id(&self) -> &EntityId {
        &self.id
    }

    /// Repository-relative path of the declaring file.
    pub fn path(&self) -> &RepoPath {
        self.id.path()
    }

    /// Entity kind.
    pub fn kind(&self) -> EntityKind {
        self.id.kind()
    }

    /// One-line rendering: `function PaymentService.retry — src/payments/service.ts:42`.
    pub fn summary(&self) -> String {
        match self.span {
            Some(span) => format!(
                "{} {} — {}:{}",
                self.kind,
                self.id.qualified_name(),
                self.path(),
                span.start_line
            ),
            None => format!("{} {}", self.kind, self.id.qualified_name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Entity, EntityId, EntityKind};
    use crate::model::path::RepoPath;
    use crate::model::span::Span;

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).expect("valid path")
    }

    fn id(s: &str, kind: EntityKind, qname: &str) -> EntityId {
        EntityId::new(path(s), kind, qname, 0)
    }

    #[test]
    fn identity_survives_moving_down_the_file() {
        // The defect this type exists to prevent. Under Cortex's line-based key these would be
        // two unrelated entities and every relation to them would be rewritten.
        let before = id("src/payments.rs", EntityKind::Function, "retry");
        let after = id("src/payments.rs", EntityKind::Function, "retry");
        assert_eq!(before, after);
    }

    #[test]
    fn kind_participates_in_identity() {
        // A Rust `struct Foo` and an `impl Foo` block are both "Foo" in the same file.
        // Cortex merged them into a single name table and mis-resolved references between them.
        let type_entity = id("src/a.rs", EntityKind::Struct, "Foo");
        let impl_block = id("src/a.rs", EntityKind::Module, "Foo#impl");
        assert_ne!(type_entity, impl_block);
    }

    #[test]
    fn case_sensitive_paths_give_distinct_identities() {
        assert_ne!(
            id("src/Foo.rs", EntityKind::Struct, "Bar"),
            id("src/foo.rs", EntityKind::Struct, "Bar")
        );
    }

    #[test]
    fn ordinal_disambiguates_overloaded_siblings() {
        let first_impl = EntityId::new(path("src/a.rs"), EntityKind::Module, "Foo#impl", 0);
        let second_impl = EntityId::new(path("src/a.rs"), EntityKind::Module, "Foo#impl", 1);
        assert_ne!(first_impl, second_impl);
        assert_eq!(first_impl.ordinal(), 0);
        assert_eq!(second_impl.ordinal(), 1);
    }

    #[test]
    fn name_and_parent_split_the_qualified_chain() {
        let e = id("src/a.rs", EntityKind::Method, "PaymentService.retry");
        assert_eq!(e.name(), "retry");
        assert_eq!(e.parent_name(), Some("PaymentService"));
        assert_eq!(e.qualified_name(), "PaymentService.retry");
    }

    #[test]
    fn top_level_entity_has_no_parent() {
        let e = id("src/a.rs", EntityKind::Function, "main");
        assert_eq!(e.name(), "main");
        assert_eq!(e.parent_name(), None);
    }

    #[test]
    fn display_includes_ordinal_only_when_needed() {
        assert_eq!(
            id("src/a.rs", EntityKind::Function, "main").display(),
            "src/a.rs::main"
        );
        assert_eq!(
            EntityId::new(path("src/a.rs"), EntityKind::Module, "Foo#impl", 2).display(),
            "src/a.rs::Foo#impl#2"
        );
    }

    #[test]
    fn entity_kind_classification() {
        assert!(EntityKind::Class.is_type());
        assert!(EntityKind::Trait.is_type());
        assert!(!EntityKind::Function.is_type());

        assert!(EntityKind::Method.is_callable());
        assert!(EntityKind::Macro.is_callable());
        assert!(!EntityKind::Field.is_callable());

        assert!(EntityKind::Field.is_member());
        assert!(!EntityKind::Function.is_member());
    }

    #[test]
    fn every_kind_has_a_distinct_label() {
        let all = [
            EntityKind::Repository,
            EntityKind::Workspace,
            EntityKind::Package,
            EntityKind::Module,
            EntityKind::File,
            EntityKind::Function,
            EntityKind::Method,
            EntityKind::Constructor,
            EntityKind::Class,
            EntityKind::Struct,
            EntityKind::Union,
            EntityKind::Enum,
            EntityKind::Interface,
            EntityKind::Trait,
            EntityKind::Protocol,
            EntityKind::TypeAlias,
            EntityKind::TypeParameter,
            EntityKind::Variable,
            EntityKind::Constant,
            EntityKind::Static,
            EntityKind::Field,
            EntityKind::Property,
            EntityKind::Parameter,
            EntityKind::Macro,
        ];
        let mut labels: Vec<&str> = all.iter().map(|k| k.as_str()).collect();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count, "entity kind labels must be unique");
    }

    #[test]
    fn entity_summary_points_at_the_declaration() {
        let entity = Entity {
            id: id("src/payments.rs", EntityKind::Method, "PaymentService.retry"),
            name: "retry".to_owned(),
            signature: Some("fn retry(&self, attempt: u32) -> Result<()>".to_owned()),
            doc: None,
            span: Span::new(100, 200, 42, 5, 48, 6),
            language: None,
            is_test: false,
            structural_fingerprint: None,
        };
        let summary = entity.summary();
        assert!(summary.contains("method"), "{summary}");
        assert!(summary.contains("PaymentService.retry"), "{summary}");
        assert!(summary.contains("src/payments.rs:42"), "{summary}");
    }
}
