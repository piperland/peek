//! Declarative, per-language extraction rules.
//!
//! # Why this exists
//!
//! Cortex advertised 28 languages. There was exactly one implementation of its extraction trait,
//! and 27 of those 28 languages were wired to a **single 16-node-type `match` arm**. The node
//! names in that arm were never checked against any grammar. Fourteen languages therefore
//! extracted zero symbols, and seven more shared *no node-type vocabulary at all* with the arm.
//! Nothing in the code could express the difference, so `cortex doctor` reported all 28 as
//! supported.
//!
//! Peek inverts that. A language's extraction behaviour is a [`LanguageSpec`]: a table of node
//! types, the fields to read, and how to reach a name that is not in a `name` field. Nothing is
//! inherited from a shared fallback, because a shared fallback is precisely what made fourteen
//! languages silently dead.
//!
//! # The enforcement mechanism
//!
//! Declaring a node type is not believing it. [`spec::assert_spec_matches_grammar`] walks the
//! real grammar and fails if any declared node type or field does not exist. That test is what
//! makes "a language is supported" a checkable statement instead of an intention — see
//! [`crate::model::language::CapabilityTier`].

use std::fmt;

use crate::model::{EntityKind, Language};

/// A node type that declares an entity, and what kind of entity it declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolRule {
    /// The exact Tree-sitter node type string, e.g. `function_item`.
    pub node_type: &'static str,
    /// The entity kind this declaration becomes.
    pub kind: EntityKind,
    /// The field holding the declared name, when the grammar has one.
    pub name_field: Option<&'static str>,
    /// How to reach the name when `name_field` is `None`.
    pub name_strategy: NameStrategy,
}

impl SymbolRule {
    pub const fn new(
        node_type: &'static str,
        kind: EntityKind,
        name_field: Option<&'static str>,
        name_strategy: NameStrategy,
    ) -> Self {
        Self {
            node_type,
            kind,
            name_field,
            name_strategy,
        }
    }
}

/// How to find a declaration's name when the grammar does not put it in a `name` field.
///
/// C and C++ are the motivating cases: `function_definition` has fields
/// `[body, declarator, type]`, and the name lives at
/// `declarator -> function_declarator -> declarator -> identifier`. Kotlin's
/// `class_declaration` has **no fields at all**, and the name is a `simple_identifier` child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NameStrategy {
    /// There is a `name` field. `NameStrategy` is unused in this case.
    #[default]
    Field,
    /// Descend through declarator chains, taking the last `identifier`-ish descendant.
    /// Correct for C and C++, where the name is wrapped in one or more declarators.
    DeclaratorChain,
    /// The name is the first `simple_identifier` child. Correct for grammars such as
    /// tree-sitter-kotlin where the declaration node carries no fields at all.
    FirstSimpleIdentifier,
    /// The name is a `type_identifier` child. Correct for Java, where a class's name is a
    /// `type_identifier` rather than a plain `identifier`.
    FirstTypeIdentifier,
}

/// A node type that represents an invocation, and the field naming the callee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallRule {
    /// The exact node type string, e.g. `call_expression`.
    pub node_type: &'static str,
    /// The field holding the callee expression, e.g. `function`.
    pub callee_field: &'static str,
}

impl CallRule {
    pub const fn new(node_type: &'static str, callee_field: &'static str) -> Self {
        Self {
            node_type,
            callee_field,
        }
    }
}

/// A node type that represents an import, and how to read the module and the bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportRule {
    pub node_type: &'static str,
    /// Field holding the module path, when the grammar has one.
    pub path_field: Option<&'static str>,
    /// Node type used to read the path when `path_field` is absent.
    pub path_node_type: Option<&'static str>,
}

impl ImportRule {
    pub const fn new(
        node_type: &'static str,
        path_field: Option<&'static str>,
        path_node_type: Option<&'static str>,
    ) -> Self {
        Self {
            node_type,
            path_field,
            path_node_type,
        }
    }
}

/// How a language expresses "this type extends or implements that one".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritanceStyle {
    /// `class A extends B` / `class A implements B, C`. Node types name the superclass and the
    /// interface list separately. Java, C#, Kotlin, Swift.
    SuperclassAndInterfaces {
        superclass_node: &'static str,
        interfaces_node: &'static str,
    },
    /// One list of base classes, as in C++'s `base_class_clause`.
    BaseList { node_type: &'static str },
    /// Trait bounds on a trait declaration's `:` clause, plus an `impl Trait for Type` block.
    /// Rust.
    TraitBounds {
        trait_decl_node: &'static str,
        bounds_node: &'static str,
        impl_node: &'static str,
        impl_trait_field: &'static str,
        impl_type_field: &'static str,
    },
    /// A `protocol` declaration with an inheritance specifier list. Swift, Kotlin.
    ProtocolInheritance {
        protocol_node: &'static str,
        inheritance_node: &'static str,
    },
}

/// A node type that represents a use of an identifier, and its node kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceRule {
    /// The exact node types that count as an identifier reference.
    pub node_types: &'static [&'static str],
    /// Node types whose children must **not** be treated as references, because the parent
    /// already accounts for them. This is what stops a call from also emitting a reference to
    /// its own callee name.
    pub excluded_parents: &'static [&'static str],
}

/// Everything Peek needs to know to extract one language.
///
/// There is no `Default` and no shared fallback. A language that has not written a spec cannot
/// be extracted, which is the intended behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageSpec {
    pub language: Language,
    /// Declarations this language's grammar can prove.
    pub symbols: &'static [SymbolRule],
    /// Invocations this language's grammar can prove.
    pub calls: &'static [CallRule],
    /// Imports this language's grammar can prove.
    pub imports: &'static [ImportRule],
    /// How this language expresses inheritance and implementation.
    pub inheritance: Option<InheritanceStyle>,
    /// Identifier uses.
    pub references: Option<ReferenceRule>,
    /// Node types that open a new ownership scope. Entities inside them get a qualified name
    /// prefixed with the enclosing entity's name.
    pub scope_nodes: &'static [&'static str],
    /// The Tree-sitter grammar for this language.
    pub grammar: fn() -> tree_sitter::Language,
}

impl LanguageSpec {
    /// Look up the spec for a language.
    ///
    /// Returns `None` for any language without a spec. Callers must treat that as
    /// "no extraction rules", never as "no symbols found".
    pub fn for_language(language: Language) -> Option<&'static LanguageSpec> {
        crate::extract::registry::all()
            .iter()
            .find(|spec| spec.language == language)
    }

    /// Whether a spec exists for this language.
    pub fn exists(language: Language) -> bool {
        Self::for_language(language).is_some()
    }

    /// The symbol rule for a node type, if this language declares one.
    pub fn symbol_rule(&self, node_type: &str) -> Option<&'static SymbolRule> {
        self.symbols.iter().find(|rule| rule.node_type == node_type)
    }

    /// The call rule for a node type, if this language has one.
    pub fn call_rule(&self, node_type: &str) -> Option<&'static CallRule> {
        self.calls.iter().find(|rule| rule.node_type == node_type)
    }

    /// The import rule for a node type, if this language has one.
    pub fn import_rule(&self, node_type: &str) -> Option<&'static ImportRule> {
        self.imports.iter().find(|rule| rule.node_type == node_type)
    }

    /// Whether this node type opens an ownership scope.
    pub fn is_scope_node(&self, node_type: &str) -> bool {
        self.scope_nodes.contains(&node_type)
    }
}

impl fmt::Display for LanguageSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} symbol rules, {} call rules, {} import rules",
            self.language,
            self.symbols.len(),
            self.calls.len(),
            self.imports.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{LanguageSpec, NameStrategy, SymbolRule};
    use crate::model::EntityKind;

    const RULES: &[SymbolRule] = &[SymbolRule::new(
        "function_item",
        EntityKind::Function,
        Some("name"),
        NameStrategy::Field,
    )];

    #[test]
    fn symbol_rules_look_up_by_node_type() {
        let spec = LanguageSpec {
            language: crate::model::Language::Rust,
            symbols: RULES,
            calls: &[],
            imports: &[],
            inheritance: None,
            references: None,
            scope_nodes: &["function_item"],
            grammar: || tree_sitter_rust::LANGUAGE.into(),
        };

        assert_eq!(
            spec.symbol_rule("function_item").map(|rule| rule.kind),
            Some(EntityKind::Function)
        );
        assert!(spec.symbol_rule("no_such_node").is_none());
        assert!(spec.is_scope_node("function_item"));
        assert!(!spec.is_scope_node("call_expression"));
        assert!(spec.summary_is_sane());
    }

    impl LanguageSpec {
        fn summary_is_sane(&self) -> bool {
            !self.to_string().is_empty() && !self.symbols.is_empty()
        }
    }
}
