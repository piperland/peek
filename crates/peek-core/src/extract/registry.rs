//! The per-language specification registry.
//!
//! Every language Peek can extract has an entry here. There is no shared fallback arm, because a
//! shared fallback is what made fourteen of Cortex's advertised languages silently dead. A
//! language without a spec simply is not extractable, and
//! [`Language::tier`](crate::model::Language::tier) will not advertise it.

use super::spec::{
    BindingRule, CallRule, ImportRule, InheritanceStyle, LanguageSpec, ModuleLayout, NameStrategy,
    ReferenceRule, SymbolRule,
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
        SymbolRule::new(
            "enum_item",
            EntityKind::Enum,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "union_item",
            EntityKind::Union,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "trait_item",
            EntityKind::Trait,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "impl_item",
            EntityKind::Module,
            Some("type"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "mod_item",
            EntityKind::Module,
            Some("name"),
            NameStrategy::Field,
        ),
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
        // `function_item`. Cortex matched neither, so it lost every trait method.
        SymbolRule::new(
            "function_signature_item",
            EntityKind::Function,
            Some("name"),
            NameStrategy::Field,
        ),
        // `enum_variant.name` is an `identifier`, not a `type_identifier`. Writing the wrong one
        // here is exactly the mistake the grammar validator exists to catch.
        SymbolRule::new(
            "enum_variant",
            EntityKind::Constant,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "associated_type",
            EntityKind::TypeAlias,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "type_parameter",
            EntityKind::TypeParameter,
            Some("name"),
            NameStrategy::Field,
        ),
        SymbolRule::new(
            "const_parameter",
            EntityKind::Parameter,
            Some("name"),
            NameStrategy::Field,
        ),
        // `parameter` has no `name` field; the name is the text of its `pattern` when the
        // pattern is a plain binding.
        SymbolRule::new(
            "parameter",
            EntityKind::Parameter,
            Some("pattern"),
            NameStrategy::Field,
        ),
    ],
    // Macro invocations are never `call_expression` in this grammar, and a Rust codebase is
    // full of them. Omitting them would understate call counts badly.
    calls: &[
        CallRule::new("call_expression", "function"),
        CallRule::new("macro_invocation", "macro"),
    ],
    imports: &[ImportRule::new(
        "use_declaration",
        Some("argument"),
        None,
        // `pub use a::B;` carries a `visibility_modifier` child. The relation is still an
        // `Imports` edge, because the resolver's R1 rung reads only `Imports` and a re-export is
        // a binding every other file in the module can be resolved against; the marker is what
        // lets the basis say so.
        &["visibility_modifier"],
    )],
    // Cortex declared `Inherit` and `Implement` in its enums and never constructed either one,
    // so its graph contained no inheritance edge at all. These are what make it possible.
    // The bounds *field* is `bounds` and the bounds *node type* is `trait_bounds` — passing one
    // where the other is expected returns `None` and loses every supertrait edge silently.
    inheritance: Some(InheritanceStyle::TraitBounds {
        trait_decl_node: "trait_item",
        bounds_field: "bounds",
        bounds_node: "trait_bounds",
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
        // `sink.text` is a `field_expression`'s `field` slot and `Sink { text: .. }` is a
        // `field_initializer`'s. Both name a member of whatever the local holds, so a name in
        // that slot is not classified by the binder that binds the receiver. Without this the
        // binding rule below would call a field read local wherever a local of the same name
        // is in scope — a correct edge replaced by a gap, which no published column shows.
        //
        // **This list is load-bearing and the gate cannot tell you so.** Emptied, every gate
        // test still passes: the fixture has no labelled row whose name is both a member and a
        // local in scope. The check that does catch it is beside the classifier —
        // `a_field_name_is_not_classified_by_a_binder_that_binds_that_name` — and it needs a
        // source the fixture does not contain.
        member_fields: &["field"],
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
    // A Rust `impl` block is a scope but is not itself a type, so it cannot be recognised via
    // `EntityKind::is_type()`. Without this list every method in a Rust codebase was extracted
    // as a bare function and the graph had no method/type distinction at all.
    type_scope_nodes: &["impl_item", "trait_item"],
    // `mod_item` is the only Rust node that declares a module. `impl_item` is deliberately
    // absent even though it is also an `EntityKind::Module` in the table above: an `impl` block
    // is a scope, not a namespace anybody can `use`.
    module_nodes: &["mod_item"],
    // The three binders whose names this index holds no entity for, spelled the way
    // `tree-sitter-rust` 0.24 spells them — every string here is checked against the real
    // grammar by `every_registered_spec_validates_against_its_real_grammar`, and a wrong one
    // fails the build rather than silently classifying nothing.
    //
    // * `let_declaration` — `pattern` carries the name, and `value` is the initialiser that is
    //   evaluated before the binding exists.
    // * `for_expression` — `pattern` carries the loop variable; `value` is the iterable, which
    //   is likewise evaluated first, and `body` is the block the variable *is* in scope in.
    // * `closure_expression` — `parameters` is a `closure_parameters` node; its children are
    //   typed `parameter` nodes or bare patterns, and only the latter bind a name this index
    //   has no entity for. The classifier asks the spec which of the two it is rather than
    //   assuming, and `return_type` names no value in the closure's own scope.
    //
    // What is deliberately **absent**: a `use ... as` alias (the resolver's R1 rung places
    // those, from the import binding), a match-arm pattern, and `if let`. A binder that is
    // not in this table is unclassified, not unlocal — see `BindingRule`.
    bindings: &[
        BindingRule::new("let_declaration", "pattern", Some("value")),
        BindingRule::new("for_expression", "pattern", Some("value")),
        BindingRule::new("closure_expression", "parameters", Some("return_type")),
    ],
    modules: Some(RUST_MODULE_LAYOUT),
    grammar: || tree_sitter_rust::LANGUAGE.into(),
};

/// Cargo's file layout, as a table.
///
/// The three lists are Cargo's own conventions and the separator is Rust's own. Nothing here is
/// a fallback shared with another language: a language whose module layout is not known has
/// `modules: None` and emits no module entities at all.
static RUST_MODULE_LAYOUT: ModuleLayout = ModuleLayout {
    source_roots: &["src"],
    package_roots: &["lib", "main"],
    directory_modules: &["mod"],
    segment_separator: "::",
};

/// Every spec Peek currently has.
static ALL: &[LanguageSpec] = &[RUST];

pub fn all() -> &'static [LanguageSpec] {
    ALL
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
        assert_eq!(
            get(Language::Rust).map(|s| s.language),
            Some(Language::Rust)
        );
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

    #[test]
    fn rust_declares_the_binders_a_local_binding_is_written_with() {
        // The table the classifier asks, asserted by name rather than by count. A count would
        // pass for a table holding the same node type twice and nothing else, and the failure
        // that matters is a *wrong* node type — which the grammar validator catches and this
        // test cannot, because it reads the same strings.
        let kinds: Vec<&str> = RUST.bindings.iter().map(|rule| rule.node_type).collect();
        assert_eq!(
            kinds,
            vec!["let_declaration", "for_expression", "closure_expression"],
            "the binders a local name can be introduced by"
        );
        for rule in RUST.bindings {
            assert!(
                !rule.name_field.is_empty(),
                "{} declares no field to read its names from, so it classifies nothing",
                rule.node_type
            );
        }
        assert!(
            RUST.references
                .as_ref()
                .is_some_and(|rule| rule.member_fields.contains(&"field")),
            "a field read is a member of what the local holds, not the local"
        );
    }
}
