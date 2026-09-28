//! The per-language specification registry.
//!
//! Every language Peek can extract has an entry here. There is no shared fallback arm, because a
//! shared fallback is what made fourteen of Cortex's advertised languages silently dead. A
//! language without a spec simply is not extractable, and
//! [`Language::tier`](crate::model::Language::tier) will not advertise it.

use super::spec::{
    CallRule, ImportRule, InheritanceStyle, LanguageSpec, NameStrategy, ReferenceRule, SymbolRule,
};
use crate::model::{EntityKind, Language};

/// Rust declarations.
///
/// The `name_field` and node-type strings here are not trusted on faith: `grammar_specs_match_
/// their_grammars` reads the real `tree-sitter-rust` language and fails the build if any of them
/// is wrong. That test is the difference between a language that is supported and a language that
/// is merely claimed.
static RUST: LanguageSpec = LanguageSpec {
    language: Language::Rust,
    symbols: &[
        SymbolRule::new(
            "function_item",
            EntityKind::Function,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "struct_item",
            EntityKind::Struct,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new("enum_item", EntityKind::Enum, Some("name"), NameStrategy::Field),
        SymbolRule::new("union_item", EntityKind::Union, Some("name"), NameStrategy::Field),
        SymbolRule::new("trait_item", EntityKind::Trait, Some("name"), NameStrategy::Field),
        SymbolRule::new("impl_item", EntityKind::Module, Some("type"), NameStrategy::Field),
        SymbolRule::new("mod_item", EntityKind::Module, Some("name"), NameStrategy::Field),
        SymbolRule::new(
            "type_item",
            EntityKind::TypeAlias,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "const_item",
            EntityKind::Constant,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "static_item",
            EntityKind::Static,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "macro_definition",
            EntityKind::Macro,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "field_declaration",
            EntityKind::Field,
            Some("name"),
            NameStrategy::Field,
        ),
        // A trait method has no body, so it is a `function_signature_item` rather than a
        // `function_item`. Cortex matched neither.
        SymbolRule::new(
            "function_signature_item",
            EntityKind::Function,
            Some("name"),
            NameStrategy::Field,
        ),
    ],
    calls: &[CallRule::new("call_expression", "function")],
    imports: &[ImportRule::new("use_declaration", Some("argument"), None)],
    // Cortex declared `Inherit` and `Implement` in its enums and never constructed either one,
    // so its graph contained no inheritance edge at all. These fields are what make it possible.
    inheritance: Some(InheritanceStyle::TraitBounds {
        trait_decl_node: "trait_item",
        bounds_node: "trait_bound",
        impl_node: "impl_item",
        impl_trait_field: "trait",
        impl_type_field: "type",
    }),
    references: Some(ReferenceRule {
        node_types: &[
            "identifier",
            "type_identifier",
            "field_identifier",
            "scoped_identifier",
        ],
        // Without this, every declaration's own name is also emitted as a reference to itself.
        excluded_parents: &[
            "function_item",
            "struct_item",
            "enum_item",
            "union_item",
            "trait_item",
            "impl_item",
            "mod_item",
            "type_item",
            "const_item",
            "static_item",
            "macro_definition",
            "field_declaration",
            "function_signature_item",
        ],
    }),
    scope_nodes: &[
        "impl_item",
        "trait_item",
        "mod_item",
        "function_item",
        "struct_item",
        "enum_item",
        "union_item",
    ],
    grammar: || tree_sitter_rust::LANGUAGE.into(),
};

/// Every spec Peek currently has.
pub fn all() -> &'static [LanguageSpec] {
    &[RUST]
}

/// Look up a spec by language.
pub fn get(language: Language) -> Option<&'static LanguageSpec> {
    all().iter().find(|spec| spec.language == language)
}

#[cfg(test)]
mod tests {
    use super::{RUST, all, get};
    use crate::extract::grammar::GrammarFacts;
    use crate::model::Language;

    #[test]
    fn rust_is_registered() {
        assert!(get(Language::Rust).is_some());
        assert_eq!(get(Language::Rust).map(|s| s.language), Some(Language::Rust));
    }

    #[test]
    fn languages_without_a_spec_are_not_claimed_as_supported() {
        // TypeScript is a first-parity *target*, not a delivered capability. There is no
        // fallback, so it is simply absent until someone writes and validates its spec.
        assert!(get(Language::TypeScript).is_none());
        assert!(!Language::TypeScript.is_advertisable());
    }

    #[test]
    fn every_registered_spec_validates_against_its_real_grammar() {
        // The mechanism that Cortex lacked. A node-type string that does not exist in the grammar
        // fails the build here instead of silently extracting nothing in production.
        for spec in all() {
            let facts = GrammarFacts::of(&(spec.grammar)());
            let problems = facts.validate(spec);
            assert!(
                problems.is_empty(),
                "spec for {} declares rules this grammar does not have:\n  {}",
                spec.language,
                problems.join("\n  ")
            );
        }
    }

    #[test]
    fn rust_spec_is_not_empty() {
        // A registered spec with no rules is the Cortex failure in a new coat.
        assert!(!RUST.symbols.is_empty(), "rust spec declares no symbols");
        assert!(!RUST.calls.is_empty(), "rust spec declares no calls");
        assert!(!RUST.imports.is_empty(), "rust spec declares no imports");
        assert!(
            RUST.inheritance.is_some(),
            "rust must be able to emit implements/inherits; Cortex never could"
        );
    }

    #[test]
    fn registry_has_no_duplicates() {
        let mut languages: Vec<Language> = all().iter().map(|spec| spec.language).collect();
        let count = languages.len();
        languages.sort_unstable();
        languages.dedup();
        assert_eq!(languages.len(), count, "a language is registered twice");
    }
}
