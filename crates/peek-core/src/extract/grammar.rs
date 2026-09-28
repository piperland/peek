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

    /// Check a whole [`LanguageSpec`] against the grammar and return every problem found.
    ///
    /// Returning all problems rather than the first is deliberate: a contributor adding a
    /// language should see the whole list of what they got wrong in one run.
    pub fn validate(&self, spec: &LanguageSpec) -> Vec<String> {
        let mut problems = Vec::new();
        let language = spec.language;

        for rule in spec.symbols {
            if !self.has_node_type(rule.node_type) {
                problems.push(format!(
                    "{language}: symbol rule names node type `{}`, which this grammar does not have",
                    rule.node_type
                ));
            }
            if let Some(field) = rule.name_field
                && !self.has_field(field)
            {
                problems.push(format!(
                    "{language}: symbol rule `{}` names field `{field}`, which this grammar does not have",
                    rule.node_type
                ));
            }
        }

        for rule in spec.calls {
            if !self.has_node_type(rule.node_type) {
                problems.push(format!(
                    "{language}: call rule names node type `{}`, which this grammar does not have",
                    rule.node_type
                ));
            }
            if !self.has_field(rule.callee_field) {
                problems.push(format!(
                    "{language}: call rule `{}` names callee field `{}`, which this grammar does not have",
                    rule.node_type, rule.callee_field
                ));
            }
        }

        for rule in spec.imports {
            if !self.has_node_type(rule.node_type) {
                problems.push(format!(
                    "{language}: import rule names node type `{}`, which this grammar does not have",
                    rule.node_type
                ));
            }
            if let Some(field) = rule.path_field
                && !self.has_field(field)
            {
                problems.push(format!(
                    "{language}: import rule `{}` names field `{field}`, which this grammar does not have",
                    rule.node_type
                ));
            }
            if let Some(node_type) = rule.path_node_type
                && !self.has_node_type(node_type)
            {
                problems.push(format!(
                    "{language}: import rule `{}` names path node type `{node_type}`, which this grammar does not have",
                    rule.node_type
                ));
            }
        }

        for node_type in spec.scope_nodes {
            if !self.has_node_type(node_type) {
                problems.push(format!(
                    "{language}: scope node `{node_type}` does not exist in this grammar"
                ));
            }
        }

        if let Some(references) = spec.references {
            for node_type in references.node_types {
                if !self.has_node_type(node_type) {
                    problems.push(format!(
                        "{language}: reference rule names node type `{node_type}`, which this grammar does not have"
                    ));
                }
            }
            for parent in references.excluded_parents {
                if !self.has_node_type(parent) {
                    problems.push(format(
                        "{language}: reference excluded-parent `{parent}` does not exist in this grammar"
                    ));
                }
            }
        }

        if let Some(style) = spec.inheritance {
            problems.extend(self.validate_inheritance(language, style));
        }

        problems
    }

    fn validate_inheritance(&self, language: crate::model::Language, style: &InheritanceStyle) -> Vec<String> {
        let mut problems = Vec::new();
        let check_node = |problems: &mut Vec<String>, node_type: &str, what: &str| {
            if !self.has_node_type(node_type) {
                problems.push(format!(
                    "{language}: inheritance style names {what} node type `{node_type}`, which this grammar does not have"
                ));
            }
        };
        let check_field = |problems: &mut Vec<String>, field: &str, what: &str| {
            if !self.has_field(field) {
                problems.push(format!(
                    "{language}: inheritance style names {what} field `{field}`, which this grammar does not have"
                ));
            }
        };

        match style {
            InheritanceStyle::SuperclassAndInterfaces {
                superclass_node,
                interfaces_node,
            } => {
                check_node(&mut problems, superclass_node, "superclass");
                check_node(&mut problems, interfaces_node, "interfaces");
            }
            InheritanceStyle::BaseList { node_type } => {
                check_node(&mut problems, node_type, "base list");
            }
            InheritanceStyle::TraitBounds {
                trait_decl_node,
                bounds_node,
                impl_node,
                impl_trait_field,
                impl_type_field,
            } => {
                check_node(&mut problems, trait_decl_node, "trait declaration");
                check_node(&mut problems, bounds_node, "trait bounds");
                check_node(&mut problems, impl_node, "impl block");
                check_field(&mut problems, impl_trait_field, "impl trait");
                check_field(&mut problems, impl_type_field, "impl type");
            }
            InheritanceStyle::ProtocolInheritance {
                protocol_node,
                inheritance_node,
            } => {
                check_node(&mut problems, protocol_node, "protocol");
                check_node(&mut problems, inheritance_node, "protocol inheritance");
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::GrammarFacts;

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
}
