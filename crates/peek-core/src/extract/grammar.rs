//! Grammar introspection and rule validation.
//!
//! # The problem this solves
//!
//! Cortex hardcoded 16 node-type strings and wired 27 languages to them. Nothing ever checked
//! those strings against the grammars they were supposed to describe. Fourteen languages
//! extracted zero symbols and the failure was invisible until someone read the grammar files by
//! hand.
//!
//! Writing a node type from memory is exactly the mistake that produced that. So Peek does not
//! trust declarations: [`GrammarFacts`] reads the real Tree-sitter language and the spec test
//! asserts that every node type and every field a [`LanguageSpec`] declares actually exists.
//!
//! A spec that names a node the grammar does not have fails the build. That is the whole point.

use std::collections::BTreeSet;

use tree_sitter::Language;

use super::spec::{InheritanceStyle, LanguageSpec};

/// The node types and field names a grammar actually contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarFacts {
    node_types: BTreeSet<String>,
    field_names: BTreeSet<String>,
}

impl GrammarFacts {
    /// Read the real node types and fields out of a Tree-sitter language.
    pub fn of(language: &Language) -> Self {
        let mut node_types = BTreeSet::new();
        let kind_count = language.node_kind_count();
        for id in 0..kind_count {
            if let Some(kind) = language.node_kind_for_id(id as u16) {
                node_types.insert(kind.to_owned());
            }
        }

        let mut field_names = BTreeSet::new();
        let field_count = language.field_count();
        for id in 0..field_count {
            if let Some(name) = language.field_name_for_id(id as u16) {
                field_names.insert(name.to_owned());
            }
        }

        Self {
            node_types,
            field_names,
        }
    }

    /// Whether the grammar has this node type.
    pub fn has_node_type(&self, node_type: &str) -> bool {
        self.node_types.contains(node_type)
    }

    /// Whether the grammar has this field name.
    pub fn has_field(&self, field: &str) -> bool {
        self.field_names.contains(field)
    }

    /// How many distinct node types the grammar declares.
    pub fn node_type_count(&self) -> usize {
        self.node_types.len()
    }

    /// Every node type, sorted. Used by conformance reports.
    pub fn node_types(&self) -> impl Iterator<Item = &str> {
        self.node_types.iter().map(String::as_str)
    }

    /// Node types whose name contains `fragment`, for use in "did you mean" diagnostics.
    ///
    /// A wrong node-type string is the single most likely mistake when writing a language spec,
    /// and it is the exact mistake that made Cortex's extractions silently dead. Pointing the
    /// author at the nearest real names turns a baffling failure into a one-line fix.
    pub fn suggest_node_types(&self, fragment: &str, limit: usize) -> Vec<&str> {
        let lowered = fragment.to_ascii_lowercase();
        self.node_types
            .iter()
            .filter(|name| name.to_ascii_lowercase().contains(&lowered))
            .map(String::as_str)
            .take(limit)
            .collect()
    }

    /// Field names containing `fragment`, for use in "did you mean" diagnostics.
    pub fn suggest_fields(&self, fragment: &str, limit: usize) -> Vec<&str> {
        let lowered = fragment.to_ascii_lowercase();
        self.field_names
            .iter()
            .filter(|name| name.to_ascii_lowercase().contains(&lowered))
            .map(String::as_str)
            .take(limit)
            .collect()
    }

    /// Format a "no such node type" problem, with the nearest real names when there are any.
    fn missing_node(
        &self,
        language: crate::model::Language,
        node_type: &str,
        what: &str,
    ) -> String {
        let suggestions = self.suggest_node_types(node_type, 8);
        if suggestions.is_empty() {
            format!(
                "{language}: {what} node type `{node_type}` does not exist in this grammar \
                 (the grammar has {} node types)",
                self.node_types.len()
            )
        } else {
            format!(
                "{language}: {what} node type `{node_type}` does not exist in this grammar; \
                 did you mean one of: {}?",
                suggestions.join(", ")
            )
        }
    }

    /// Format a "no such field" problem, with the nearest real names when there are any.
    fn missing_field(&self, language: crate::model::Language, field: &str, what: &str) -> String {
        let suggestions = self.suggest_fields(field, 8);
        if suggestions.is_empty() {
            format!(
                "{language}: {what} field `{field}` does not exist in this grammar \
                 (the grammar has {} fields: {})",
                self.field_names.len(),
                self.field_names
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "{language}: {what} field `{field}` does not exist in this grammar; \
                 did you mean one of: {}?",
                suggestions.join(", ")
            )
        }
    }

    /// Check a whole [`LanguageSpec`] against the grammar and return every problem found.
    ///
    /// Returning all problems rather than the first is deliberate: a contributor adding a
    /// language should see the whole list of what they got wrong in one run.
    pub fn validate(&self, spec: &LanguageSpec) -> Vec<String> {
        let mut problems = Vec::new();
        let language = spec.language;

        for rule in spec.symbols {
            if !self.has_node_type(rule.node_type) {
                problems.push(self.missing_node(language, rule.node_type, "symbol rule"));
            }
            if let Some(field) = rule.name_field
                && !self.has_field(field)
            {
                problems.push(self.missing_field(language, field, "symbol rule name"));
            }
        }

        for rule in spec.calls {
            if !self.has_node_type(rule.node_type) {
                problems.push(self.missing_node(language, rule.node_type, "call rule"));
            }
            if !self.has_field(rule.callee_field) {
                problems.push(self.missing_field(language, rule.callee_field, "callee"));
            }
        }

        for rule in spec.imports {
            if !self.has_node_type(rule.node_type) {
                problems.push(self.missing_node(language, rule.node_type, "import rule"));
            }
            if let Some(field) = rule.path_field
                && !self.has_field(field)
            {
                problems.push(self.missing_field(language, field, "import path"));
            }
            if let Some(node_type) = rule.path_node_type
                && !self.has_node_type(node_type)
            {
                problems.push(self.missing_node(language, node_type, "import path"));
            }
        }

        for node_type in spec.scope_nodes {
            if !self.has_node_type(node_type) {
                problems.push(self.missing_node(language, node_type, "scope"));
            }
        }

        for node_type in spec.type_scope_nodes {
            if !self.has_node_type(node_type) {
                problems.push(self.missing_node(language, node_type, "type scope"));
            }
        }

        for node_type in spec.module_nodes {
            if !self.has_node_type(node_type) {
                problems.push(self.missing_node(language, node_type, "module declaration"));
            }
        }

        // A binder is a node type *and* two fields, and all three are grammar data. A wrong
        // field name returns `None` from `child_by_field_name` and the classifier then treats
        // the binder as introducing nothing — a silent no-op, which is the failure mode this
        // file exists to make loud.
        for rule in spec.bindings {
            if !self.has_node_type(rule.node_type) {
                problems.push(self.missing_node(language, rule.node_type, "binding"));
            }
            if !self.has_field(rule.name_field) {
                problems.push(self.missing_field(language, rule.name_field, "binding names"));
            }
            if let Some(field) = rule.not_in_force
                && !self.has_field(field)
            {
                problems.push(self.missing_field(
                    language,
                    field,
                    "binding not yet in force",
                ));
            }
        }

        // `spec.modules` is deliberately not validated here. A `ModuleLayout` holds directory
        // names and a path separator, not grammar node types, and there is no grammar to check
        // them against — it is checked by the module tests, which assert the *resulting* names
        // against real file paths. Validating the absence of nothing would only look like the
        // same enforcement the rest of this file provides.

        if let Some(references) = spec.references {
            for node_type in references.node_types {
                if !self.has_node_type(node_type) {
                    problems.push(self.missing_node(language, node_type, "reference"));
                }
            }
            for field in references.member_fields {
                if !self.has_field(field) {
                    problems.push(self.missing_field(language, field, "reference member"));
                }
            }
        }

        if let Some(style) = spec.inheritance {
            problems.extend(self.validate_inheritance(language, &style));
        }

        problems
    }

    fn validate_inheritance(
        &self,
        language: crate::model::Language,
        style: &InheritanceStyle,
    ) -> Vec<String> {
        let mut problems = Vec::new();
        match style {
            InheritanceStyle::SuperclassAndInterfaces {
                superclass_node,
                interfaces_node,
            } => {
                if !self.has_node_type(superclass_node) {
                    problems.push(self.missing_node(language, superclass_node, "superclass"));
                }
                if !self.has_node_type(interfaces_node) {
                    problems.push(self.missing_node(language, interfaces_node, "interfaces"));
                }
            }
            InheritanceStyle::BaseList { node_type } => {
                if !self.has_node_type(node_type) {
                    problems.push(self.missing_node(language, node_type, "base list"));
                }
            }
            InheritanceStyle::TraitBounds {
                trait_decl_node,
                bounds_field,
                bounds_node,
                impl_node,
                impl_trait_field,
                impl_type_field,
            } => {
                if !self.has_node_type(trait_decl_node) {
                    problems.push(self.missing_node(
                        language,
                        trait_decl_node,
                        "trait declaration",
                    ));
                }
                if !self.has_node_type(bounds_node) {
                    problems.push(self.missing_node(language, bounds_node, "trait bounds"));
                }
                if !self.has_field(bounds_field) {
                    problems.push(self.missing_field(language, bounds_field, "trait bounds"));
                }
                if !self.has_node_type(impl_node) {
                    problems.push(self.missing_node(language, impl_node, "impl block"));
                }
                if !self.has_field(impl_trait_field) {
                    problems.push(self.missing_field(language, impl_trait_field, "impl trait"));
                }
                if !self.has_field(impl_type_field) {
                    problems.push(self.missing_field(language, impl_type_field, "impl type"));
                }
            }
            InheritanceStyle::ProtocolInheritance {
                protocol_node,
                inheritance_node,
            } => {
                if !self.has_node_type(protocol_node) {
                    problems.push(self.missing_node(language, protocol_node, "protocol"));
                }
                if !self.has_node_type(inheritance_node) {
                    problems.push(self.missing_node(
                        language,
                        inheritance_node,
                        "protocol inheritance",
                    ));
                }
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::GrammarFacts;
    use crate::extract::spec::{BindingRule, LanguageSpec};
    use crate::model::Language;

    fn rust() -> LanguageSpec {
        *LanguageSpec::for_language(Language::Rust).expect("rust has a spec")
    }

    #[test]
    fn reads_real_node_types_from_a_real_grammar() {
        let facts = GrammarFacts::of(&tree_sitter_rust::LANGUAGE.into());
        assert!(facts.node_type_count() > 50, "rust grammar should be large");
        assert!(facts.has_node_type("function_item"));
        assert!(facts.has_node_type("call_expression"));
        assert!(facts.has_node_type("use_declaration"));
        assert!(!facts.has_node_type("no_such_node_type"));
    }

    #[test]
    fn reads_real_field_names() {
        let facts = GrammarFacts::of(&tree_sitter_rust::LANGUAGE.into());
        assert!(facts.has_field("name"));
        assert!(facts.has_field("type"));
        assert!(facts.has_field("function"));
        assert!(!facts.has_field("no_such_field"));
    }

    #[test]
    fn a_binding_node_type_the_grammar_does_not_have_fails_the_build() {
        // The mechanism that keeps the table from rotting, proved by asking the validator
        // about a misspelling rather than by trusting that it would notice one. A classifier
        // keyed on a node type the grammar has never heard of fires **never**, and a rule
        // that fires never looks exactly like a rule that does no harm.
        let facts = GrammarFacts::of(&tree_sitter_rust::LANGUAGE.into());
        let mut spec = rust();
        spec.bindings = &[BindingRule::new("let_declaraton", "pattern", Some("value"))];
        let problems = facts.validate(&spec);
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("let_declaraton")),
            "a misspelt binding node type must be reported, not accepted: {problems:?}"
        );
    }

    #[test]
    fn a_binding_field_the_grammar_does_not_have_fails_the_build() {
        // The quieter half of the same rot. `child_by_field_name("patern")` returns `None`,
        // every binder in the table then introduces nothing, and the classifier classifies
        // nothing at all — which no count of damage would show.
        let facts = GrammarFacts::of(&tree_sitter_rust::LANGUAGE.into());
        let mut spec = rust();
        spec.bindings = &[BindingRule::new("let_declaration", "patern", None)];
        let problems = facts.validate(&spec);
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("patern")),
            "a misspelt binding field must be reported, not accepted: {problems:?}"
        );
    }
}
