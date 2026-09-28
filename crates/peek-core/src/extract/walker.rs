//! The extraction walker: parse tree in, entities and relations out.
//!
//! The walker is entirely driven by a [`LanguageSpec`]. It contains **no per-language
//! knowledge** — every node type it looks at was declared in a spec and validated against the
//! real grammar. That separation is what makes adding a language a data change rather than a
//! code change, and it is the direct opposite of Cortex, whose 27-language extractor was one
//! hardcoded `match` arm that silently matched nothing for most of them.
//!
//! # Three things this walker is careful about
//!
//! 1. **Receivers survive.** Cortex extracted a call's target as "the last identifier in the
//!    callee subtree", so `a.b.c()` became `"c"` and the receiver was gone before resolution
//!    could use it. Here the callee is decomposed into a name plus an optional receiver or
//!    path, and the receiver is carried into the relation's evidence.
//!
//! 2. **A broken parse is visible.** Cortex had no `ERROR` handling at all, so a file full of
//!    syntax errors was extracted partially and then recorded as *fully indexed*. A file with
//!    parse errors is recorded as [`Degraded`] here.
//!
//! 3. **Ambiguity is not guessed away.** `S::new()` is byte-identical to `mod::new()` in Rust.
//!    The walker marks such a call [`UnresolvedReason::Ambiguous`] rather than pretending to
//!    know which it was.

use std::collections::HashMap;

use tree_sitter::Node;

use super::source::SourceText;
use super::spec::{InheritanceStyle, LanguageSpec, NameStrategy, SymbolRule};
use crate::model::{
    Entity, EntityId, EntityKind, Evidence, Language, Relation, RelationKind, ResolutionState,
    RepoPath, Span, UnresolvedReason,
};

/// What is wrong with a file, if anything.
///
/// Reported rather than hidden. Cortex counted a syntactically invalid file as `indexed_files`
/// and reported `indexed_files: 0` as a successful index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degradation {
    /// Number of `ERROR` nodes in the tree.
    pub error_nodes: usize,
    /// Number of `MISSING` nodes in the tree.
    pub missing_nodes: usize,
    /// First error byte offset, for pointing a human at it.
    pub first_error_byte: Option<u32>,
}

impl Degradation {
    /// A one-line, human-readable summary.
    pub fn describe(&self) -> String {
        format!(
            "parse produced {} error node(s) and {} missing node(s); extraction is best effort",
            self.error_nodes, self.missing_nodes
        )
    }
}

/// The result of extracting one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedFile {
    pub path: RepoPath,
    pub language: Language,
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
    /// `None` when the file parsed cleanly. A degraded file must never be reported as clean.
    pub degradation: Option<Degradation>,
}

impl ExtractedFile {
    /// Whether the file parsed cleanly.
    pub fn is_clean(&self) -> bool {
        self.degradation.is_none()
    }

    /// Counts by entity kind, for `peek doctor` and benchmarks.
    pub fn entity_counts(&self) -> HashMap<EntityKind, usize> {
        let mut counts = HashMap::new();
        for entity in &self.entities {
            *counts.entry(entity.kind()).or_insert(0) += 1;
        }
        counts
    }

    /// Counts by relation kind.
    pub fn relation_counts(&self) -> HashMap<RelationKind, usize> {
        let mut counts = HashMap::new();
        for relation in &self.relations {
            *counts.entry(relation.kind).or_insert(0) += 1;
        }
        counts
    }
}

/// A callee decomposed into the parts a resolver actually needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Callee {
    /// The final, called name — `retry` in `service.retry()`.
    name: String,
    /// The receiver expression — `service` in `service.retry()`.
    receiver: Option<String>,
    /// The qualifying path — `crate::payments` in `crate::payments::new()`.
    path: Option<String>,
    /// True when the callee is computed, so no static target exists.
    dynamic: bool,
}

impl Callee {
    /// The evidence this callee implies, so the resolver starts from something better than a
    /// bare name.
    ///
    /// This is the fix for Cortex's core defect: it resolved every call through one repo-global
    /// name table, so `a.render()` in a file that also declared an unrelated `render` bound to
    /// the wrong method, silently. Here the receiver is evidence the resolver can weigh.
    fn evidence(&self) -> (Evidence, bool) {
        if self.dynamic {
            return (Evidence::NameOnly, true);
        }
        match (&self.receiver, &self.path) {
            (Some(receiver), None) => (
                Evidence::ReceiverType {
                    receiver: receiver.clone(),
                },
                false,
            ),
            (None, Some(path)) => {
                if path.split("::").count() > 1 {
                    (
                        Evidence::QualifiedNameInScope {
                            scope: path.clone(),
                        },
                        false,
                    )
                } else {
                    // `S::new()` and `mod::new()` are byte-identical. A single-segment path is
                    // genuinely ambiguous and is not resolved here.
                    (Evidence::NameOnly, true)
                }
            }
            _ => (Evidence::NameOnly, false),
        }
    }
}

/// One entry on the ownership scope stack.
struct ScopeEntry {
    name: String,
    id: EntityId,
    /// Whether the enclosing declaration is a type, which is what promotes a function declared
    /// inside it to a method.
    is_type: bool,
}

/// The walker.
struct Walker<'a> {
    spec: &'static LanguageSpec,
    source: SourceText<'a>,
    path: RepoPath,
    /// The file entity every module-level relation anchors on.
    file_id: EntityId,
    entities: Vec<Entity>,
    relations: Vec<Relation>,
    scope: Vec<ScopeEntry>,
    /// Ordinals for entities sharing a kind and qualified name, so `impl Foo` blocks stay
    /// distinct identities without making line numbers part of the key.
    ordinals: HashMap<(EntityKind, String), u32>,
}

impl<'a> Walker<'a> {
    fn new(spec: &'static LanguageSpec, path: RepoPath, text: &'a str) -> Self {
        let file_id = EntityId::new(
            path.clone(),
            EntityKind::File,
            path.file_name().to_owned(),
            0,
        );
        Self {
            spec,
            source: SourceText::new(text),
            file_id: file_id.clone(),
            entities: vec![Entity {
                id: file_id,
                name: path.file_name().to_owned(),
                signature: None,
                doc: None,
                span: None,
                language: Some(spec.language),
                is_test: false,
                structural_fingerprint: None,
            }],
            path,
            relations: Vec::new(),
            scope: Vec::new(),
            ordinals: HashMap::new(),
        }
    }

    fn span(&self, node: Node<'_>) -> Span {
        match self.source.span(node.start_byte(), node.end_byte()) {
            Some(span) => span,
            // A Tree-sitter range is always well-formed and in bounds, so this is unreachable in
            // practice. A zero-width span at the last line keeps a pathological grammar from
            // aborting an entire file over a position calculation.
            None => {
                let byte = u32::try_from(node.start_byte()).unwrap_or(0);
                let line = u32::try_from(self.source.line_count()).unwrap_or(1);
                Span {
                    start_byte: byte,
                    end_byte: byte,
                    start_line: line,
                    start_column: 1,
                    end_line: line,
                    end_column: 1,
                }
            }
        }
    }

    fn text(&self, node: Node<'_>) -> Option<String> {
        self.source.text().get(node.byte_range()).map(str::trim).map(str::to_owned)
    }

    /// The entity a relation should originate from: the innermost enclosing scope, or nothing if
    /// we are at file level.
    fn current_entity(&self) -> Option<&EntityId> {
        self.scope.last().map(|entry| &entry.id)
    }

    fn next_ordinal(&mut self, kind: EntityKind, qualified_name: &str) -> u32 {
        let counter = self
            .ordinals
            .entry((kind, qualified_name.to_owned()))
            .or_insert(0);
        let value = *counter;
        *counter += 1;
        value
    }

    fn qualified_name(&self, name: &str) -> String {
        if self.scope.is_empty() {
            name.to_owned()
        } else {
            let prefix = self
                .scope
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(".");
            format!("{prefix}.{name}")
        }
    }

    /// The name of a declaration, following the spec's strategy.
    fn declaration_name(&self, node: Node<'_>, rule: &SymbolRule) -> Option<String> {
        if let Some(field) = rule.name_field
            && let Some(child) = node.child_by_field_name(field)
            && let Some(text) = self.text(child)
            && !text.is_empty()
        {
            return Some(text);
        }

        match rule.name_strategy {
            NameStrategy::Field => None,
            // C and C++: the name is wrapped in one or more declarator nodes.
            NameStrategy::DeclaratorChain => {
                let mut current = node;
                for _ in 0..8 {
                    if matches!(
                        current.kind(),
                        "identifier" | "type_identifier" | "field_identifier"
                    ) {
                        return self.text(current);
                    }
                    let Some(child) = current.named_child(0) else {
                        return None;
                    };
                    current = child;
                }
                None
            }
            // Kotlin and similar: the declaration carries no fields, and the name is a
            // `simple_identifier` child.
            NameStrategy::FirstSimpleIdentifier => node
                .named_children(&mut node.walk())
                .find(|child| child.kind() == "simple_identifier")
                .and_then(|child| self.text(child)),
            // Java: a class's name is a `type_identifier` child, not a plain `identifier`.
            NameStrategy::FirstTypeIdentifier => node
                .named_children(&mut node.walk())
                .find(|child| child.kind() == "type_identifier")
                .and_then(|child| self.text(child)),
        }
    }

    /// Doc comments preceding a node, skipping over attributes.
    ///
    /// Doc comments are `extra` nodes in Tree-sitter, so they are *siblings before* the item
    /// rather than children of it, and an `#[attribute]` can sit between them.
    fn doc_comment(&self, node: Node<'_>) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut current = node.prev_named_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" | "inner_attribute_item" => {
                    current = sibling.prev_named_sibling();
                }
                "line_comment" | "block_comment" if sibling.child_by_field_name("doc").is_some() => {
                    if let Some(text) = self.text(sibling) {
                        // Strip the leading `///`, `//!` or `/** */` marker.
                        let cleaned = text
                            .trim_start_matches('/')
                            .trim_start_matches('!')
                            .trim_start_matches('*')
                            .trim_end_matches("*/")
                            .trim();
                        lines.push(cleaned.to_owned());
                    }
                    current = sibling.prev_named_sibling();
                }
                _ => break,
            }
        }
        if lines.is_empty() {
            return None;
        }
        lines.reverse();
        Some(lines.join("\n"))
    }

    /// The names a node introduces as type bounds.
    ///
    /// Handles both shapes the grammars use. A `trait_bounds` node *contains* the bounds, so
    /// its children are read. But the `impl_item.trait` field is already the bound itself, and
    /// reading its children would find nothing — a `type_identifier` has no children. Getting
    /// this wrong is why Cortex could not produce a single implementation edge.
    fn bound_names(&self, node: Node<'_>) -> Vec<String> {
        match node.kind() {
            "trait_bounds" | "use_bounds" => node
                .named_children(&mut node.walk())
                .filter_map(|child| self.type_name(child))
                .collect(),
            _ => self.type_name(node).into_iter().collect(),
        }
    }

    /// The type named by an `impl_item`'s `type` field, reading through generic and reference
    /// wrappers to the underlying name.
    fn impl_type_name(&self, impl_node: Node<'_>) -> Option<String> {
        let target = impl_node.child_by_field_name("type")?;
        self.type_name(target)
    }

    /// Reduce a type expression to a readable name.
    fn type_name(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "type_identifier" => self.text(node),
            "scoped_type_identifier" => {
                let name = node.child_by_field_name("name")?;
                let prefix = node.child_by_field_name("path").and_then(|p| self.text(p));
                match prefix {
                    Some(prefix) => Some(format!("{prefix}::{}", self.text(name)?)),
                    None => self.text(name),
                }
            }
            "generic_type" => {
                let inner = node.child_by_field_name("type")?;
                self.type_name(inner)
            }
            "reference_type" | "pointer_type" => {
                let inner = node.named_children(&mut node.walk()).find(|child| {
                    !matches!(child.kind(), "lifetime" | "type_arguments")
                })?;
                self.type_name(inner)
            }
            "dynamic_type" | "abstract_type" => {
                // `impl dyn Trait` carries its bound here. Reading only `impl_item.trait` loses
                // it silently, which is a documented trap in this grammar.
                let trait_node = node.child_by_field_name("trait")?;
                self.type_name(trait_node)
            }
            _ => None,
        }
    }

    /// Decompose a callee expression, preserving the receiver and the qualifying path.
    fn callee(&self, node: Node<'_>) -> Callee {
        match node.kind() {
            "identifier" | "field_identifier" | "type_identifier" => Callee {
                name: self.text(node).unwrap_or_default(),
                receiver: None,
                path: None,
                dynamic: false,
            },
            "field_expression" => {
                let field = node.child_by_field_name("field");
                let value = node.child_by_field_name("value");
                Callee {
                    name: field
                        .and_then(|node| self.text(node))
                        .unwrap_or_default(),
                    receiver: value.and_then(|node| self.text(node)),
                    path: None,
                    dynamic: false,
                }
            }
            "scoped_identifier" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|node| self.text(node))
                    .unwrap_or_default();
                let path = node.child_by_field_name("path").and_then(|node| self.text(node));
                Callee {
                    name,
                    receiver: None,
                    path,
                    dynamic: false,
                }
            }
            // `x.method::<T>()` — the real callee is inside.
            "generic_function" => node
                .child_by_field_name("function")
                .map_or_else(
                    || self.dynamic_callee(node),
                    |inner| self.callee(inner),
                ),
            // These have no fields; the interesting child is positional.
            "try_expression" | "await_expression" | "parenthesized_expression" => node
                .named_child(0)
                .map_or_else(|| self.dynamic_callee(node), |inner| self.callee(inner)),
            "index_expression" => {
                let value = node.named_child(0);
                let index = node.named_child(1);
                Callee {
                    name: index
                        .and_then(|node| self.text(node))
                        .unwrap_or_default(),
                    receiver: value.and_then(|node| self.text(node)),
                    path: None,
                    dynamic: false,
                }
            }
            "call_expression" => node
                .child_by_field_name("function")
                .map_or_else(|| self.dynamic_callee(node), |inner| self.callee(inner)),
            _ => self.dynamic_callee(node),
        }
    }

    fn dynamic_callee(&self, node: Node<'_>) -> Callee {
        Callee {
            name: self.text(node).unwrap_or_else(|| "<computed>".to_owned()),
            receiver: None,
            path: None,
            dynamic: true,
        }
    }

    /// Walk the tree.
    fn run(&mut self, root: Node<'_>) {
        self.walk(root);
    }

    fn walk(&mut self, node: Node<'_>) {
        // Comments and attributes carry no declarations or relations worth keeping, and
        // descending into them only adds noise.
        if matches!(node.kind(), "line_comment" | "block_comment") {
            return;
        }

        let declared = self.declare(node);
        if let Some(id) = declared.clone() {
            let is_type = id.kind().is_type();
            self.scope.push(ScopeEntry {
                name: id.name().to_owned(),
                id,
                is_type,
            });
        }

        self.emit_relations(node);

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child);
        }

        if declared.is_some() {
            self.scope.pop();
        }
    }

    /// Create an entity for a declaration node, if the spec declares one.
    fn declare(&mut self, node: Node<'_>) -> Option<EntityId> {
        let rule = self.spec.symbol_rule(node.kind())?;
        let name = self.declaration_name(node, rule)?;
        if name.is_empty() {
            return None;
        }

        // A function declared inside a type is a method, not a plain function. The language
        // spec cannot express this — `function_item` means one node type in every context — so
        // the distinction is made here, from the scope stack, rather than being declared twice
        // in every language's table.
        let mut kind = rule.kind;
        if kind == EntityKind::Function
            && let Some(enclosing) = self.scope.last()
            && enclosing.is_type
        {
            kind = EntityKind::Method;
        }

        let qualified_name = self.qualified_name(&name);
        let ordinal = self.next_ordinal(kind, &qualified_name);
        let id = EntityId::new(self.path.clone(), kind, qualified_name, ordinal);
        let span = self.span(node);

        let entity = Entity {
            id: id.clone(),
            name: name.clone(),
            signature: self.signature(node, rule),
            doc: self.doc_comment(node),
            span: Some(span),
            language: Some(self.spec.language),
            // Peek has no framework rules yet, so a `#[test]`-annotated function is the only
            // test signal available from source alone. Detect it conservatively.
            is_test: self.looks_like_test(node),
            structural_fingerprint: None,
        };
        self.entities.push(entity);

        // Containment is expressed as relations rather than a side table, so that every edge in
        // the graph has exactly one representation.
        if let Some(parent) = self.scope.last().map(|entry| entry.id.clone()) {
            self.relations.push(Relation::resolved(
                RelationKind::Contains,
                parent,
                id.clone(),
                name,
                span,
                Evidence::Containment,
            ));
        }
        Some(id)
    }

    /// A signature, when the node has a parameters list and an optional return type.
    ///
    /// Verbatim from the source. Never synthesised, and `None` when it cannot be read.
    fn signature(&self, node: Node<'_>, rule: &SymbolRule) -> Option<String> {
        if !matches!(rule.kind, EntityKind::Function | EntityKind::Method) {
            return None;
        }
        let parameters = node.child_by_field_name("parameters")?;
        let head = self.source.text().get(node.byte_range())?;
        let start = head.find('(')?;
        let end = self.source.text().get(parameters.byte_range())?.find(')')? + parameters.start_byte() - node.start_byte() + 1;
        let mut signature = head.get(start..end)?.trim().to_owned();
        if let Some(return_type) = node.child_by_field_name("return_type")
            && let Some(text) = self.source.text().get(return_type.byte_range())
        {
            signature.push_str(" -> ");
            signature.push_str(text.trim());
        }
        Some(signature)
    }

    /// Whether a function carries a test attribute.
    fn looks_like_test(&self, node: Node<'_>) -> bool {
        let mut current = node.prev_named_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" | "inner_attribute_item" => {
                    if let Some(text) = self.text(sibling)
                        && (text.contains("test") || text.contains("bench"))
                    {
                        return true;
                    }
                    current = sibling.prev_named_sibling();
                }
                "line_comment" | "block_comment" => current = sibling.prev_named_sibling(),
                _ => break,
            }
        }
        false
    }

    /// Emit the non-structural relations a node implies.
    fn emit_relations(&mut self, node: Node<'_>) {
        // A `use` declaration and an `impl` block sit outside any function body, so they have
        // no enclosing symbol to originate from. They anchor on the file instead of being
        // dropped — Cortex silently lost every module-level import for the same reason.
        let subject = self.current_entity().cloned().unwrap_or_else(|| self.file_id.clone());

        if let Some(rule) = self.spec.call_rule(node.kind())
            && let Some(callee_node) = node.child_by_field_name(rule.callee_field)
        {
            let callee = self.callee(callee_node);
            self.emit_call(subject.clone(), node, &callee);
        }

        if let Some(rule) = self.spec.import_rule(node.kind()) {
            self.emit_import(subject.clone(), node, rule.path_field);
        }

        if let Some(style) = self.spec.inheritance {
            self.emit_inheritance(node, style);
        }

        if let Some(references) = self.spec.references
            && references.node_types.contains(&node.kind())
            && !self.is_excluded_reference(node, references.excluded_parents)
            && self.current_entity().is_some()
        {
            self.emit_reference(subject, node);
        }
    }
    /// Whether a node's parent excludes its children from reference extraction.
    ///
    /// Without this, every declaration's own name is emitted as a reference to itself.
    fn is_excluded_reference(&self, node: Node<'_>, excluded: &[&str]) -> bool {
        let mut current = node;
        // Walk up to the nearest declared ancestor, not just the immediate parent: a call's
        // callee identifier is a grandchild of the function node.
        while let Some(parent) = current.parent() {
            if self.spec.symbol_rule(parent.kind()).is_some() {
                return excluded.contains(&parent.kind());
            }
            current = parent;
        }
        false
    }

    fn emit_call(&mut self, source: EntityId, node: Node<'_>, callee: &Callee) {
        if callee.name.is_empty() {
            return;
        }
        let span = self.span(node);
        let (evidence, ambiguous) = callee.evidence();

        if ambiguous {
            // `S::new()` could be a type-associated call or a module-qualified one. Report the
            // ambiguity rather than guessing.
            self.relations.push(Relation::unresolved(
                RelationKind::Calls,
                source,
                callee.name.clone(),
                span,
                UnresolvedReason::Ambiguous,
            ));
        } else {
            self.relations.push(Relation::pending(
                RelationKind::Calls,
                source,
                callee.name.clone(),
                span,
                evidence,
            ));
        }
    }

    /// Emit `imports` relations from a `use` declaration, preserving aliases.
    fn emit_import(&mut self, source: EntityId, node: Node<'_>, path_field: Option<&str>) {
        let Some(argument) = path_field.and_then(|field| node.child_by_field_name(field)) else {
            return;
        };
        // Copy the text out so the binding walk does not borrow `self` while we push relations.
        let text = self.source.text().to_owned();
        let span = self.span(node);
        let mut bindings = Vec::new();
        collect_use_bindings(text.as_str(), argument, "", &mut bindings);
        for binding in bindings {
            self.relations.push(Relation::pending(
                RelationKind::Imports,
                source.clone(),
                binding.local,
                span,
                Evidence::ImportBinding {
                    module: binding.module,
                    alias: binding.alias,
                },
            ));
        }
    }

    /// Emit `implements` and `inherits` relations.
    fn emit_inheritance(&mut self, node: Node<'_>, style: InheritanceStyle) {
        match style {
            InheritanceStyle::TraitBounds {
                trait_decl_node,
                bounds_node,
                impl_node,
                impl_trait_field,
                impl_type_field,
            } => {
                // `impl Trait for Type` -> implements.
                if node.kind() == impl_node {
                    let implemented = self.impl_type_name(node);
                    if let Some(implemented) = implemented {
                        let span = self.span(node);
                        if let Some(trait_node) = node.child_by_field_name(impl_trait_field) {
                            for bound in self.bound_names(trait_node) {
                                self.emit_named(
                                    RelationKind::Implements,
                                    &implemented,
                                    &bound,
                                    span,
                                    "inherits",
                                );
                            }
                        } else if let Some(target) = node.child_by_field_name(impl_type_field) {
                            // `impl dyn Trait {}` hides the bound inside the type.
                            if matches!(target.kind(), "dynamic_type" | "abstract_type")
                                && let Some(bound) = target
                                    .child_by_field_name("trait")
                                    .and_then(|node| self.type_name(node))
                            {
                                self.emit_named(
                                    RelationKind::Implements,
                                    &implemented,
                                    &bound,
                                    span,
                                    "inherits",
                                );
                            }
                        }
                    }
                }

                // `trait X: Y` -> inherits.
                if node.kind() == trait_decl_node {
                    let name = node
                        .child_by_field_name("name")
                        .and_then(|child| self.text(child));
                    if let (Some(name), Some(bounds)) =
                        (name, node.child_by_field_name(bounds_node))
                    {
                        let span = self.span(node);
                        for bound in self.bound_names(bounds) {
                            self.emit_named(
                                RelationKind::Inherits,
                                &name,
                                &bound,
                                span,
                                "inherits",
                            );
                        }
                    }
                }
            }
            InheritanceStyle::SuperclassAndInterfaces {
                superclass_node,
                interfaces_node,
            } => {
                if let Some(name) = self.declared_name(node)
                    && let Some(parent) = self.child_with_kind(node, superclass_node)
                    && let Some(parent_name) = self.text(parent)
                {
                    self.emit_named(
                        RelationKind::Inherits,
                        &name,
                        &parent_name,
                        self.span(node),
                        "inherits",
                    );
                }
                if let Some(name) = self.declared_name(node)
                    && let Some(list) = self.child_with_kind(node, interfaces_node)
                {
                    let span = self.span(node);
                    for interface in self.bound_names(list) {
                        self.emit_named(
                            RelationKind::Implements,
                            &name,
                            &interface,
                            span,
                            "inherits",
                        );
                    }
                }
            }
            InheritanceStyle::BaseList { node_type } => {
                if let Some(name) = self.declared_name(node)
                    && let Some(list) = self.child_with_kind(node, node_type)
                {
                    let span = self.span(node);
                    for base in self.bound_names(list) {
                        self.emit_named(RelationKind::Inherits, &name, &base, span, "inherits");
                    }
                }
            }
            InheritanceStyle::ProtocolInheritance {
                protocol_node,
                inheritance_node,
            } => {
                if let Some(name) = self.declared_name(node)
                    && let Some(list) = self.child_with_kind(node, inheritance_node)
                {
                    let span = self.span(node);
                    for parent in self.bound_names(list) {
                        self.emit_named(RelationKind::Inherits, &name, &parent, span, "inherits");
                    }
                }
                let _ = protocol_node;
            }
        }
    }

    /// Emit an inheritance relation that the resolver will bind to a real entity.
    ///
    /// The subject is a *name*, not an `EntityId`, because inheritance clauses name a type that
    /// this file may not declare. The resolver turns the name into an entity or records why it
    /// could not.
    fn emit_named(
        &mut self,
        kind: RelationKind,
        subject: &str,
        bound: &str,
        span: Span,
        basis: &str,
    ) {
        if subject.is_empty() || bound.is_empty() {
            return;
        }
        // Anchor on the enclosing entity, or the file when there is none.
        let anchor = self
            .current_entity()
            .cloned()
            .unwrap_or_else(|| self.file_id.clone());
        self.relations.push(Relation {
            kind,
            source: anchor,
            target_name: bound.to_owned(),
            target: None,
            span,
            resolution: ResolutionState::Inferred {
                by: Evidence::NameOnly,
                basis: format!("{basis} relation on `{subject}`"),
            },
        });
    }

    fn emit_reference(&mut self, source: EntityId, node: Node<'_>) {
        if let Some(name) = self.text(node)
            && !name.is_empty()
        {
            self.relations.push(Relation::pending(
                RelationKind::References,
                source,
                name,
                self.span(node),
                Evidence::NameOnly,
            ));
        }
    }

    /// The declared name of a node, if this spec declares it as a symbol.
    fn declared_name(&self, node: Node<'_>) -> Option<String> {
        let rule = self.spec.symbol_rule(node.kind())?;
        self.declaration_name(node, rule)
    }

    /// The first child of the given kind.
    ///
    /// The returned node borrows the tree, not `self`, so the signature names the tree's
    /// lifetime explicitly.
    fn child_with_kind<'t>(&self, node: Node<'t>, kind: &str) -> Option<Node<'t>> {
        node.named_children(&mut node.walk())
            .find(|child| child.kind() == kind)
    }
}

/// A single name introduced by a `use` declaration.
struct UseBinding {
    /// The name the symbol is bound to locally.
    local: String,
    /// The full module path it came from.
    module: String,
    /// The explicit alias, when the import renamed it.
    alias: Option<String>,
}

/// Walk a `use` argument, collecting every name it binds.
///
/// Takes the source text rather than the whole walker, so its lifetime is independent of the
/// walker's borrow of that text.
fn collect_use_bindings(source: &str, node: Node<'_>, prefix: &str, out: &mut Vec<UseBinding>) {
    let text_of = |node: Node<'_>| -> Option<String> {
        source.get(node.byte_range()).map(str::trim).map(str::to_owned)
    };

    match node.kind() {
        "use_as_clause" => {
            let path = node
                .child_by_field_name("path")
                .and_then(text_of)
                .unwrap_or_default();
            let alias = node.child_by_field_name("alias").and_then(text_of);
            let module = join_path(prefix, &path);
            if let Some(alias) = alias {
                out.push(UseBinding {
                    local: alias.clone(),
                    module,
                    alias: Some(alias),
                });
            } else if let Some(name) = last_segment(&path) {
                out.push(UseBinding {
                    local: name,
                    module,
                    alias: None,
                });
            }
        }
        "scoped_identifier" => {
            let name = node
                .child_by_field_name("name")
                .and_then(text_of)
                .unwrap_or_default();
            let path_prefix = node
                .child_by_field_name("path")
                .and_then(text_of)
                .unwrap_or_default();
            let full = join_path(prefix, &join_path(&path_prefix, &name));
            if let Some(last) = last_segment(&name) {
                out.push(UseBinding {
                    local: last,
                    module: full,
                    alias: None,
                });
            }
        }
        // `use a::{b, c};`
        "scoped_use_list" => {
            let base = node
                .child_by_field_name("path")
                .and_then(text_of)
                .unwrap_or_default();
            let combined = join_path(prefix, &base);
            if let Some(list) = node.child_by_field_name("list") {
                for child in list.named_children(&mut list.walk()) {
                    collect_use_bindings(source, child, &combined, out);
                }
            }
        }
        "use_list" => {
            for child in node.named_children(&mut node.walk()) {
                collect_use_bindings(source, child, prefix, out);
            }
        }
        // `use a::*;` — the path is an unnamed child, not a field.
        "use_wildcard" => {
            let base = text_of(node).unwrap_or_default();
            if !base.is_empty() {
                out.push(UseBinding {
                    local: "*".to_owned(),
                    module: join_path(prefix, &base),
                    alias: None,
                });
            }
        }
        "identifier" | "type_identifier" | "crate" | "self" | "super" | "metavariable" => {
            let path = text_of(node).unwrap_or_default();
            let full = join_path(prefix, &path);
            if let Some(local) = last_segment(&path) {
                out.push(UseBinding {
                    local,
                    module: full,
                    alias: None,
                });
            }
        }
        _ => {}
    }
}

fn join_path(prefix: &str, suffix: &str) -> String {
    if prefix.is_empty() {
        suffix.to_owned()
    } else if suffix.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}::{suffix}")
    }
}

fn last_segment(path: &str) -> Option<String> {
    path.rsplit("::").next().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Count `ERROR` and `MISSING` nodes in a tree.
fn count_defects(node: Node<'_>, errors: &mut usize, missing: &mut usize) -> Option<usize> {
    let mut first_error = if node.is_error() || node.is_missing() {
        Some(node.start_byte())
    } else {
        None
    };
    if node.is_error() {
        *errors += 1;
    }
    if node.is_missing() {
        *missing += 1;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(byte) = count_defects(child, errors, missing) {
            first_error = Some(first_error.map_or(byte, |current| current.min(byte)));
        }
    }
    first_error
}

/// Extract a file using the spec registered for `path`'s language.
///
/// Returns `None` when no spec is registered for the language. Callers must treat that as
/// "no extraction rules for this language", never as "this file has no symbols" — conflating the
/// two is how fourteen of Cortex's languages reported as supported while extracting nothing.
pub fn extract(path: RepoPath, text: &str) -> Option<ExtractedFile> {
    let language = Language::from_extension(path.extension()?.as_str())?;
    let spec = crate::extract::registry::get(language)?;
    Some(extract_with(spec, path, text))
}

/// Extract a file with an explicit spec.
pub fn extract_with(spec: &'static LanguageSpec, path: RepoPath, text: &str) -> ExtractedFile {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&(spec.grammar)()).is_err() {
        // A grammar that cannot be set is a programming error in the spec, not a runtime
        // condition. The registry test already asserts every spec's grammar loads.
        return ExtractedFile {
            path,
            language: spec.language,
            entities: Vec::new(),
            relations: Vec::new(),
            degradation: Some(Degradation {
                error_nodes: 1,
                missing_nodes: 0,
                first_error_byte: Some(0),
            }),
        };
    }

    let Some(tree) = parser.parse(text, None) else {
        return ExtractedFile {
            path,
            language: spec.language,
            entities: Vec::new(),
            relations: Vec::new(),
            degradation: Some(Degradation {
                error_nodes: 1,
                missing_nodes: 0,
                first_error_byte: Some(0),
            }),
        };
    };

    let mut errors = 0usize;
    let mut missing = 0usize;
    let first_error = count_defects(tree.root_node(), &mut errors, &mut missing);

    let mut walker = Walker::new(spec, path, text);
    walker.run(tree.root_node());

    ExtractedFile {
        path: walker.path,
        language: spec.language,
        entities: walker.entities,
        relations: walker.relations,
        degradation: (errors > 0 || missing > 0).then_some(Degradation {
            error_nodes: errors,
            missing_nodes: missing,
            first_error_byte: first_error.map(|byte| byte.min(u32::MAX as usize) as u32),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_with, ExtractedFile};
    use crate::extract::registry;
    use crate::model::{EntityKind, RelationKind, RepoPath, ResolutionState};

    fn path() -> RepoPath {
        RepoPath::new("src/payments.rs").expect("valid path")
    }

    fn rust(source: &str) -> ExtractedFile {
        extract_with(registry::get(crate::model::Language::Rust).expect("rust spec"), path(), source)
    }

    fn qualified_names(file: &ExtractedFile) -> Vec<String> {
        let mut names: Vec<String> = file
            .entities
            .iter()
            .map(|entity| entity.id.qualified_name().to_owned())
            .collect();
        names.sort();
        names
    }

    fn relation_count(file: &ExtractedFile, kind: RelationKind) -> usize {
        file.relations.iter().filter(|r| r.kind == kind).count()
    }

    #[test]
    fn extracts_top_level_items_with_qualified_names() {
        let file = rust(
            r#"
            struct PaymentService;
            fn retry() {}
            const MAX: u32 = 3;
            "#,
        );
        assert!(file.is_clean(), "{:?}", file.degradation);
        let names = qualified_names(&file);
        assert!(names.contains(&"PaymentService".to_owned()), "{names:?}");
        assert!(names.contains(&"retry".to_owned()), "{names:?}");
        assert!(names.contains(&"MAX".to_owned()), "{names:?}");

        let service = file
            .entities
            .iter()
            .find(|entity| entity.name == "PaymentService")
            .expect("struct extracted");
        assert_eq!(service.kind(), EntityKind::Struct);
        assert_eq!(service.language, Some(crate::model::Language::Rust));
    }

    #[test]
    fn impl_block_creates_a_scope_so_methods_get_qualified_names() {
        // This is what makes `qualified_name` carry information. A `struct Foo` and its
        // `impl Foo` methods are distinguished by scope, not by line number.
        let file = rust(
            r#"
            struct PaymentService;
            impl PaymentService {
                fn new() -> Self { Self }
                fn retry(&self) { }
            }
            "#,
        );
        let names = qualified_names(&file);
        assert!(names.contains(&"PaymentService.new".to_owned()), "{names:?}");
        assert!(
            names.contains(&"PaymentService.retry".to_owned()),
            "{names:?}"
        );
    }

    #[test]
    fn multiple_impl_blocks_stay_distinct_entities() {
        // Cortex collapsed these into one name table and mis-resolved references between them.
        let file = rust(
            r#"
            struct S;
            impl S { fn a(&self) {} }
            impl S { fn b(&self) {} }
            "#,
        );
        let impl_methods: Vec<&str> = file
            .entities
            .iter()
            .filter(|entity| entity.kind() == EntityKind::Method)
            .map(|entity| entity.id.qualified_name())
            .collect();
        assert_eq!(impl_methods.len(), 2, "{impl_methods:?}");
        assert!(impl_methods.contains(&"S.a"));
        assert!(impl_methods.contains(&"S.b"));

        // And they have different identities despite sharing a name prefix.
        let a = file
            .entities
            .iter()
            .find(|entity| entity.id.qualified_name() == "S.a")
            .expect("S.a");
        let b = file
            .entities
            .iter()
            .find(|entity| entity.id.qualified_name() == "S.b")
            .expect("S.b");
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn trait_methods_without_a_body_are_extracted() {
        // `function_signature_item`, not `function_item`. Cortex matched only the latter and
        // therefore lost every required trait method.
        let file = rust(
            r#"
            trait Gateway {
                fn charge(&self, amount: u32) -> Result<(), Error>;
            }
            "#,
        );
        let names = qualified_names(&file);
        assert!(names.contains(&"Gateway.charge".to_owned()), "{names:?}");
    }

    #[test]
    fn impl_trait_for_type_emits_implements() {
        // Cortex declared `Implements` in its enum and never constructed one, so its graphs
        // contained no implementation edges at all.
        let file = rust(
            r#"
            trait Gateway { fn charge(&self); }
            struct Stripe;
            impl Gateway for Stripe { fn charge(&self) {} }
            "#,
        );
        let implements: Vec<&str> = file
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Implements)
            .map(|r| r.target_name.as_str())
            .collect();
        assert_eq!(implements, vec!["Gateway"], "{implements:?}");
    }

    #[test]
    fn supertraits_emit_inherits() {
        let file = rust(
            r#"
            trait Base {}
            trait Extended: Base + Send {}
            "#,
        );
        let inherits: Vec<&str> = file
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Inherits)
            .map(|r| r.target_name.as_str())
            .collect();
        assert!(inherits.contains(&"Base"), "{inherits:?}");
    }

    #[test]
    fn impl_dyn_trait_is_not_lost() {
        // The bound lives at `impl_item.trait`. Reading only the `type` field loses it silently,
        // which is a documented trap in this grammar.
        let file = rust("impl std::fmt::Debug for MyType {}");
        let implements: Vec<&str> = file
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Implements)
            .map(|r| r.target_name.as_str())
            .collect();
        assert_eq!(implements, vec!["std::fmt::Debug"], "{implements:?}");
    }

    #[test]
    fn impl_dyn_marker_type_keeps_its_bound() {
        // `impl dyn Trait {}` hides the bound at `type` -> `dynamic_type` -> `trait`.
        let file = rust("impl dyn std::fmt::Debug for MyType {}");
        let implements: Vec<&str> = file
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Implements)
            .map(|r| r.target_name.as_str())
            .collect();
        assert_eq!(implements, vec!["std::fmt::Debug"], "{implements:?}");
    }

    #[test]
    fn call_receivers_are_preserved_as_evidence() {
        // The core fix. Cortex resolved `service.retry()` by the bare name `retry` against a
        // repo-global table, so it bound to whichever `retry` sorted first in the repository.
        let file = rust(
            r#"
            struct Service;
            impl Service { fn retry(&self) {} }
            fn main() {
                let service = Service;
                service.retry();
            }
            "#,
        );
        let call = file
            .relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls && r.target_name == "retry")
            .expect("the call to retry");

        match &call.resolution {
            ResolutionState::Pending { evidence } => match evidence {
                crate::model::Evidence::ReceiverType { receiver } => {
                    assert_eq!(receiver, "service", "the receiver must survive extraction");
                }
                other => panic!("expected receiver evidence, got {other:?}"),
            },
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[test]
    fn single_segment_path_calls_are_reported_as_ambiguous() {
        // `S::new()` and `mod::new()` are byte-identical in Rust. Claiming to know which one it
        // is would be a confident wrong answer.
        let file = rust("struct S; impl S { fn new() -> S { S } } fn main() { S::new(); }");
        let call = file
            .relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls && r.target_name == "new")
            .expect("the call to new");
        assert_eq!(
            call.resolution,
            ResolutionState::Unresolved {
                reason: crate::model::UnresolvedReason::Ambiguous
            },
            "a one-segment path call must be ambiguous, not guessed"
        );
    }

    #[test]
    fn multi_segment_path_calls_carry_scope_evidence() {
        let file = rust("fn main() { crate::payments::service::new(); }");
        let call = file
            .relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls && r.target_name == "new")
            .expect("the call");
        assert_eq!(call.resolution.evidence_class(), Some("qualified_name_in_scope"));
    }

    #[test]
    fn macro_invocations_are_calls() {
        // `println!` is a `macro_invocation`, never a `call_expression`, and a Rust codebase is
        // full of them. Omitting them would badly understate call counts.
        let file = rust("fn main() { println!(\"hi\"); }");
        assert!(relation_count(&file, RelationKind::Calls) >= 1, "macro call missing");
    }

    #[test]
    fn imports_capture_the_module_and_the_alias() {
        // Cortex's `ExtractedRelation` had no alias field, so `use a::B as C` lost the mapping
        // at extraction time and the resolver had nothing to work with.
        let file = rust(
            r#"
            mod inner {}
            use crate::inner::Service as Svc;
            use crate::inner::Other;
            "#,
        );
        let imports: Vec<_> = file
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Imports)
            .collect();
        assert_eq!(imports.len(), 2, "{imports:?}");

        let aliased = imports
            .iter()
            .find(|r| r.target_name == "Svc")
            .expect("the aliased import");
        match &aliased.resolution {
            ResolutionState::Pending { evidence } => match evidence {
                crate::model::Evidence::ImportBinding { module, alias } => {
                    assert_eq!(module, "crate::inner::Service");
                    assert_eq!(alias.as_deref(), Some("Svc"));
                }
                other => panic!("expected import binding, got {other:?}"),
            },
            other => panic!("expected pending, got {other:?}"),
        }

        let plain = imports
            .iter()
            .find(|r| r.target_name == "Other")
            .expect("the plain import");
        assert_eq!(plain.resolution.evidence_class(), Some("import_binding"));
    }

    #[test]
    fn nested_use_lists_produce_one_binding_per_name() {
        let file = rust("use crate::models::{Alpha, Beta as B};");
        let locals: Vec<&str> = file
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Imports)
            .map(|r| r.target_name.as_str())
            .collect();
        assert!(locals.contains(&"Alpha"), "{locals:?}");
        assert!(locals.contains(&"B"), "{locals:?}");
    }

    #[test]
    fn a_broken_parse_is_reported_as_degraded() {
        // Cortex had no ERROR handling at all, so a file full of syntax errors was extracted
        // partially and then counted as a fully indexed file.
        let file = rust("fn broken( { this is not rust");
        assert!(!file.is_clean(), "a broken parse must not be clean");
        let degradation = file.degradation.as_ref().expect("degradation recorded");
        assert!(degradation.error_nodes > 0, "{degradation:?}");
        assert!(!degradation.describe().is_empty());
    }

    #[test]
    fn clean_source_produces_no_degradation() {
        let file = rust("fn ok() {}");
        assert!(file.is_clean());
        assert!(file.degradation.is_none());
    }

    #[test]
    fn doc_comments_attach_to_the_following_item() {
        // Doc comments are `extra` siblings *before* the item, with attributes sometimes between.
        let file = rust(
            r#"
            /// Retries the payment.
            /// Returns the new attempt number.
            fn retry() {}
            "#,
        );
        let retry = file
            .entities
            .iter()
            .find(|entity| entity.name == "retry")
            .expect("retry extracted");
        let doc = retry.doc.as_deref().expect("doc comment attached");
        assert!(doc.contains("Retries the payment."), "{doc}");
        assert!(doc.contains("new attempt number."), "{doc}");
    }

    #[test]
    fn test_functions_are_flagged() {
        let file = rust(
            r#"
            #[test]
            fn it_works() {}
            fn not_a_test() {}
            "#,
        );
        let test = file
            .entities
            .iter()
            .find(|entity| entity.name == "it_works")
            .expect("extracted");
        let other = file
            .entities
            .iter()
            .find(|entity| entity.name == "not_a_test")
            .expect("extracted");
        assert!(test.is_test);
        assert!(!other.is_test);
    }

    #[test]
    fn signatures_are_read_verbatim_and_never_invented() {
        let file = rust("fn charge(amount: u32) -> Result<(), Error> { todo!() }");
        let charge = file
            .entities
            .iter()
            .find(|entity| entity.name == "charge")
            .expect("extracted");
        let signature = charge.signature.as_deref().expect("signature read");
        assert!(signature.contains("amount: u32"), "{signature}");
        assert!(signature.contains("Result<(), Error>"), "{signature}");

        // A struct has no signature, and none is invented for it.
        let other = rust("struct S;");
        let s = other.entities.first().expect("extracted");
        assert!(s.signature.is_none());
    }

    #[test]
    fn containment_is_a_relation_not_a_side_table() {
        let file = rust("struct S; impl S { fn m(&self) {} }");
        let contains = relation_count(&file, RelationKind::Contains);
        assert_eq!(contains, 1, "the method is contained in the impl block");
    }

    #[test]
    fn summary_helpers_count_by_kind() {
        let file = rust("struct S; fn f() { g(); } fn g() {}");
        let entities = file.entity_counts();
        assert_eq!(entities.get(&EntityKind::Function), Some(&2));
        assert_eq!(entities.get(&EntityKind::Struct), Some(&1));
        assert_eq!(entities.get(&EntityKind::File), Some(&1));

        let relations = file.relation_counts();
        assert!(relations.contains_key(&RelationKind::Calls), "{relations:?}");
    }

    #[test]
    fn a_file_entity_exists_so_module_level_relations_have_an_anchor() {
        // A `use` declaration and an `impl` block sit outside any function body. Without a file
        // entity they have nowhere to originate from and are silently lost, which is what
        // happened to every module-level import in the code Peek replaces.
        let file = rust("use std::collections::HashMap; trait T {} struct S; impl T for S {}");
        let file_entity = file
            .entities
            .iter()
            .find(|entity| entity.kind() == EntityKind::File)
            .expect("a file entity is always present");
        assert_eq!(file_entity.name, "payments.rs");

        let import = file
            .relations
            .iter()
            .find(|r| r.kind == RelationKind::Imports)
            .expect("the import survived");
        assert_eq!(import.source, file_entity.id);
    }

    #[test]
    fn diagnostic_dump_grammar_shapes() {
        // Temporary: prints the real parse tree for constructs whose shape we are unsure of, so
        // the extractor is written against observed output rather than assumption.
        for source in [
            "impl dyn std::fmt::Debug for MyType {}",
            "trait Extended: Base + Send {}",
            "impl Gateway for Stripe {}",
        ] {
            let mut parser = tree_sitter::Parser::new();
            let spec = registry::get(crate::model::Language::Rust).expect("rust spec");
            if parser.set_language(&(spec.grammar)()).is_err() {
                continue;
            }
            let Some(tree) = parser.parse(source, None) else {
                continue;
            };
            let mut out = String::new();
            // Print node kinds and field names only; that is what the extractor depends on.
            fn print(node: tree_sitter::Node<'_>, depth: usize, out: &mut String) {
                let field = node.parent().and_then(|parent| {
                    let mut cursor = parent.walk();
                    (0..parent.child_count())
                        .find(|i| parent.child(*i) == Some(node))
                        .and_then(|i| {
                            parent
                                .field_name_for_child(i as u32)
                                .map(str::to_owned)
                        })
                });
                out.push_str(&format!(
                    "{}{}{}{}\n",
                    "  ".repeat(depth),
                    field.map_or_else(String::new, |f| format!("{f}: ")),
                    node.kind(),
                    if node.is_named() { "" } else { " (anon)" }
                ));
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    print(child, depth + 1, out);
                }
            }
            print(tree.root_node(), 0, &mut out);
            println!("=== {source}\n{out}");
        }
    }

    #[test]
    fn unknown_language_yields_none_not_an_empty_file() {
        // Conflating "no rules" with "no symbols" is how Cortex reported 14 dead languages as
        // supported.
        let markdown = RepoPath::new("README.md").expect("valid path");
        assert!(super::extract(markdown, "# hi").is_none());
    }
}
