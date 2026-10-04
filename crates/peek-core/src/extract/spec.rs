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
    /// Node types that, appearing among the import's children, make it a **re-export** rather
    /// than a private import — Rust's `visibility_modifier` on `pub use`.
    ///
    /// A re-export is still a binding in the referring module, so it is still an `Imports`
    /// relation: the resolver's R1 rung reads *only* `Imports`, and a re-export replaced by a
    /// differently-typed edge would stop being a name that anything in the file can be resolved
    /// against. What the marker buys is the distinction in the relation's basis, so `peek
    /// explain` can say "this name is part of the module's public surface" without re-parsing
    /// the file. It is a marker list rather than a boolean because "how does a language say an
    /// import is public" is a question with a different answer in every grammar.
    pub reexport_markers: &'static [&'static str],
}

impl ImportRule {
    pub const fn new(
        node_type: &'static str,
        path_field: Option<&'static str>,
        path_node_type: Option<&'static str>,
        reexport_markers: &'static [&'static str],
    ) -> Self {
        Self {
            node_type,
            path_field,
            path_node_type,
            reexport_markers,
        }
    }
}

/// How a language turns a file's path into the module path that file has inside its package.
///
/// This is **data, and it is absent for a language whose module convention is not yet known**.
/// `Option::None` means "Peek cannot place this language's files in a namespace", and the
/// extractor then emits no module entities at all. That is the honest answer, and it is the
/// direct opposite of what the engine Peek replaces did: it manufactured a module from any
/// string containing a dot, a slash or a hyphen, so `java.util.List` became a real symbol and
/// any kebab-cased JavaScript identifier matched (audit B7).
///
/// # The rule it encodes
///
/// A file's module is named by its path **relative to its package's source root**, prefixed by
/// the package. The source root is the first path component named in [`Self::source_roots`]
/// (`src` for Cargo), or the file's own directory when there is none. The prefix is the name of
/// the directory above the source root, because that is the directory Cargo names the package
/// after.
///
/// Both halves are approximations and both are stated rather than hidden. See
/// [`crate::extract::modules`] for what each one gets wrong and why it is still better than no
/// module table at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleLayout {
    /// Directory names that begin a package's own source tree, e.g. `src`.
    ///
    /// The **first** such component in a path wins. A nested `src` further down is a directory
    /// that happens to be called `src`, not a second package root, and treating it as one would
    /// give every file under it a qualified name that no `use` statement can ever spell.
    pub source_roots: &'static [&'static str],
    /// File stems that *are* the package's root module, e.g. `lib` and `main`.
    ///
    /// Only meaningful for a file sitting directly in the source root: `src/a/main.rs` is the
    /// module `pkg::a::main`, not a second package.
    pub package_roots: &'static [&'static str],
    /// File stems that mean "this file is the module its **directory** is named after", e.g. `mod`.
    ///
    /// Rust has no separate directory entity: `src/a/mod.rs` *is* the module `a`, and a
    /// directory with no `mod.rs` is not a module at all. A language where a directory and the
    /// file inside it are different namespaces needs a different table entry.
    pub directory_modules: &'static [&'static str],
    /// The separator used to join a module's segments, e.g. `::`.
    ///
    /// Spelled the way the *source* spells it, so a qualified name can be compared with a `use`
    /// path without rewriting one into the other.
    pub segment_separator: &'static str,
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
    ///
    /// `bounds_field` is a **field name** (used with `child_by_field_name`) and `bounds_node` is
    /// a **node type** (used for validation). They are different strings and conflating them is
    /// an easy mistake with a silent result: asking for a field named `trait_bounds` when the
    /// grammar names it `bounds` returns `None`, and every supertrait edge is quietly lost.
    TraitBounds {
        trait_decl_node: &'static str,
        bounds_field: &'static str,
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
    ///
    /// Every occurrence of one of these is a use of a name **except** a declaration's own
    /// name, which the walker decides for itself — see
    /// [`LanguageSpec::symbols`] and the walker's own-name test. There is deliberately no
    /// second list here: a list of node types whose children are not references is a rule
    /// about whole subtrees, and it cannot say which occurrence inside the subtree
    /// introduced the name.
    pub node_types: &'static [&'static str],
}

/// Everything Peek needs to know to extract one language.
///
/// There is no `Default` and no shared fallback. A language that has not written a spec cannot
/// be extracted, which is the intended behaviour.
///
/// Note the deliberate absence of `PartialEq`: the struct carries a `fn` pointer to the grammar,
/// and comparing function pointers is meaningless because their addresses are not guaranteed to
/// be distinct. Compare [`Self::language`] instead.
#[derive(Debug, Clone, Copy)]
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
    /// Node types whose scope is a **type**, so a function declared inside one is a method
    /// rather than a plain function.
    ///
    /// This is separate from `scope_nodes` and from `EntityKind::is_type()` because a Rust
    /// `impl` block is a scope but is not itself a type — it is the *inherent or trait
    /// implementation of* one. Without this, every method in a Rust codebase was extracted as
    /// a bare `Function` and no method/type distinction existed in the graph at all.
    pub type_scope_nodes: &'static [&'static str],
    /// Node types that declare a **module**, as distinct from a symbol that merely has
    /// `EntityKind::Module`.
    ///
    /// The distinction has to be in the spec because the entity kind cannot carry it: the Rust
    /// table maps both `impl_item` and `mod_item` to `EntityKind::Module`, since an `impl` block
    /// is the nearest thing Rust has to a namespace. Keying the walker off the kind would
    /// therefore treat every `impl` block in a codebase as a module declaration, and a `mod`
    /// keyword appearing in the shared walker is exactly the thing this table exists to prevent.
    pub module_nodes: &'static [&'static str],
    /// How this language's files map onto modules and packages, or `None` when that is not yet
    /// known.
    pub modules: Option<ModuleLayout>,
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

    /// Whether this node type opens a *type* scope, so functions inside it are methods.
    pub fn is_type_scope_node(&self, node_type: &str) -> bool {
        self.type_scope_nodes.contains(&node_type)
    }

    /// Whether this node type declares a module.
    pub fn is_module_node(&self, node_type: &str) -> bool {
        self.module_nodes.contains(&node_type)
    }

    /// The module layout for this language, or `None` when its file-to-module convention is
    /// not known. Callers must treat that as "no module structure for this language", never as
    /// "this file has no modules".
    ///
    /// By value rather than by reference: `ModuleLayout` is a `Copy` table of `&'static` slices,
    /// and handing out a `&'static` to a field of a `&self` would borrow a local for the whole
    /// of the caller's program.
    pub fn module_layout(&self) -> Option<ModuleLayout> {
        self.modules
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
    use super::{CallRule, ImportRule, LanguageSpec, ModuleLayout, NameStrategy, SymbolRule};
    use crate::model::EntityKind;

    const RULES: &[SymbolRule] = &[SymbolRule::new(
        "function_item",
        EntityKind::Function,
        Some("name"),
        NameStrategy::Field,
    )];
    const CALLS: &[CallRule] = &[CallRule::new("call_expression", "function")];
    const IMPORTS: &[ImportRule] = &[ImportRule::new(
        "use_declaration",
        Some("argument"),
        None,
        &["visibility_modifier"],
    )];

    fn spec() -> LanguageSpec {
        LanguageSpec {
            language: crate::model::Language::Rust,
            symbols: RULES,
            calls: CALLS,
            imports: IMPORTS,
            inheritance: None,
            references: None,
            scope_nodes: &["function_item"],
            type_scope_nodes: &["function_item"],
            module_nodes: &["mod_item"],
            modules: Some(ModuleLayout {
                source_roots: &["src"],
                package_roots: &["lib", "main"],
                directory_modules: &["mod"],
                segment_separator: "::",
            }),
            grammar: || tree_sitter_rust::LANGUAGE.into(),
        }
    }

    #[test]
    fn symbol_rules_look_up_by_node_type() {
        let spec = spec();
        assert_eq!(
            spec.symbol_rule("function_item").map(|rule| rule.kind),
            Some(EntityKind::Function)
        );
        assert!(spec.symbol_rule("no_such_node").is_none());
    }

    #[test]
    fn call_and_import_rules_look_up_by_node_type() {
        let spec = spec();
        assert_eq!(
            spec.call_rule("call_expression")
                .map(|rule| rule.callee_field),
            Some("function")
        );
        assert!(spec.call_rule("no_such_node").is_none());
        assert_eq!(
            spec.import_rule("use_declaration")
                .and_then(|rule| rule.path_field),
            Some("argument")
        );
        assert!(spec.import_rule("no_such_node").is_none());
    }

    #[test]
    fn an_import_rule_carries_the_markers_that_make_it_a_re_export() {
        // A language that says "this import is public" in its grammar declares the node type
        // here. Nothing in the walker knows what `pub` is.
        let spec = spec();
        assert_eq!(
            spec.import_rule("use_declaration")
                .map(|rule| rule.reexport_markers),
            Some(["visibility_modifier"].as_slice())
        );
    }

    #[test]
    fn a_module_declaration_is_told_apart_from_something_that_is_merely_kinded_module() {
        // Both are `EntityKind::Module` in the Rust table, because an `impl` block is the
        // nearest thing Rust has to a namespace. Only the table can say which one is a module.
        let spec = spec();
        assert!(spec.is_module_node("mod_item"));
        assert!(
            !spec.is_module_node("impl_item"),
            "an impl block is a scope, not a module declaration"
        );
        assert!(!spec.is_module_node("struct_item"));
    }

    #[test]
    fn a_language_without_a_module_convention_says_so() {
        // `None` is the answer for every language whose file-to-module layout is not yet
        // known, and the extractor then emits no module entities. A language must not inherit
        // another language's convention, which is the same mistake as a shared fallback match
        // arm.
        let mut without = spec();
        without.modules = None;
        assert!(without.module_layout().is_none());
    }

    #[test]
    fn scope_nodes_are_recognised() {
        let spec = spec();
        assert!(spec.is_scope_node("function_item"));
        assert!(!spec.is_scope_node("call_expression"));
    }

    #[test]
    fn type_scope_nodes_are_recognised_separately() {
        let spec = spec();
        assert!(spec.is_type_scope_node("function_item"));
        assert!(
            !spec.is_type_scope_node("call_expression"),
            "a call is not a type scope"
        );
    }

    #[test]
    fn display_summarises_the_spec() {
        let rendered = spec().to_string();
        assert!(rendered.starts_with("rust:"), "{rendered}");
        assert!(rendered.contains("1 symbol rules"), "{rendered}");
    }
}
