//! The extraction walker: parse tree in, entities and relations out.
//!
//! The walker is entirely driven by a [`LanguageSpec`]. It contains **no per-language
//! knowledge** — every node type it looks at was declared in a spec and validated against the
//! real grammar. That separation is what makes adding a language a data change rather than a
//! code change, and it is the direct opposite of Cortex, whose 27-language extractor was one
//! hardcoded `match` arm that silently matched nothing for most of them.
//!
//! # Four things this walker is careful about
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
//!
//! 4. **Adding structure does not disturb what already works.** A file's module structure is
//!    emitted *alongside* the declarations rather than folded into them, and an inline `mod x`
//!    keeps the qualified name it always had. A module is a declaration, not a reference, and
//!    nothing here rewrites a symbol's identity to accommodate one: `qualified_name` is two
//!    thirds of `EntityId`, so re-keying it would churn every method in the repository over a
//!    refactor that moved nothing.

use std::collections::HashMap;

use tree_sitter::Node;

use super::modules;
use super::source::SourceText;
use super::spec::{ImportRule, InheritanceStyle, LanguageSpec, NameStrategy, SymbolRule};
use crate::model::{
    Entity, EntityId, EntityKind, Evidence, Language, Relation, RelationKind, RepoPath, Span,
    UnresolvedReason,
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
    /// The module and package entities this extraction contributed, in the order they were
    /// created.
    ///
    /// They are in [`Self::entities`] like everything else, because a consumer that filtered
    /// them out would lose the module table. This list is here so a consumer can tell a module
    /// *node* from a declaration node without re-deriving the file-to-module convention and
    /// without inferring it from a name's shape — which is how a declaration ends up mistaken
    /// for a namespace.
    pub module_ids: Vec<EntityId>,
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
    /// The module this file *is*, when the spec has a module layout. `None` for a language
    /// whose file-to-module convention is not known, which is a different thing from "this file
    /// declares no module".
    module_id: Option<EntityId>,
    /// Every module and package entity this walk created, carried out on [`ExtractedFile`].
    module_ids: Vec<EntityId>,
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
            module_id: None,
            module_ids: Vec::new(),
            scope: Vec::new(),
            ordinals: HashMap::new(),
        }
    }

    fn span(&self, node: Node<'_>) -> Span {
        self.span_of(node.start_byte()..node.end_byte())
    }

    /// The span of a byte range, with a last-resort fallback for a range the source cannot be
    /// asked about.
    fn span_of(&self, range: std::ops::Range<usize>) -> Span {
        match self.source.span(range.start, range.end) {
            Some(span) => span,
            // A Tree-sitter range is always well-formed and in bounds, so this is unreachable in
            // practice. A zero-width span at the last line keeps a pathological grammar from
            // aborting an entire file over a position calculation.
            None => {
                let byte = u32::try_from(range.start).unwrap_or(0);
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
        self.source
            .text()
            .get(node.byte_range())
            .map(str::trim)
            .map(str::to_owned)
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
                    current = current.named_child(0)?;
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
                "line_comment" | "block_comment"
                    if sibling.child_by_field_name("doc").is_some() =>
                {
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
                let inner = node
                    .named_children(&mut node.walk())
                    .find(|child| !matches!(child.kind(), "lifetime" | "type_arguments"))?;
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
                    name: field.and_then(|node| self.text(node)).unwrap_or_default(),
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
                let path = node
                    .child_by_field_name("path")
                    .and_then(|node| self.text(node));
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
                .map_or_else(|| self.dynamic_callee(node), |inner| self.callee(inner)),
            // These have no fields; the interesting child is positional.
            "try_expression" | "await_expression" | "parenthesized_expression" => node
                .named_child(0)
                .map_or_else(|| self.dynamic_callee(node), |inner| self.callee(inner)),
            "index_expression" => {
                let value = node.named_child(0);
                let index = node.named_child(1);
                Callee {
                    name: index.and_then(|node| self.text(node)).unwrap_or_default(),
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
        // Module structure first, so a module declaration declared later in the file has a
        // parent to be contained by. The entities are appended to the same list as everything
        // else; nothing downstream has to know they came from a different pass.
        self.emit_file_modules();
        self.walk(root);
    }

    /// Emit the package and module this file contributes, before any declaration is walked.
    fn emit_file_modules(&mut self) {
        let spec = self.spec;
        if spec.module_layout().is_none() {
            return;
        }
        let text = self.source.text();
        let found = modules::for_file(
            spec,
            &self.path,
            &self.file_id,
            self.span_of(0..text.len()),
            text,
        );
        self.module_id = found.module.clone();
        for entity in found.entities {
            self.module_ids.push(entity.id.clone());
            self.entities.push(entity);
        }
        self.relations.extend(found.relations);
    }

    fn walk(&mut self, node: Node<'_>) {
        // Comments and attributes carry no declarations or relations worth keeping, and
        // descending into them only adds noise.
        if matches!(node.kind(), "line_comment" | "block_comment") {
            return;
        }

        let declared = self.declare(node);
        if let Some(id) = declared.clone() {
            // A scope is a "type scope" if the spec says so, or if the entity is itself a type.
            let is_type = self.spec.is_type_scope_node(node.kind()) || id.kind().is_type();
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

        // A module declaration is the one kind whose body is worth hashing, and only the spec can
        // say which node types those are: the Rust table maps `impl_item` to `EntityKind::Module`
        // as well, and keying this off the kind would fingerprint every impl block in a codebase
        // as though it were a namespace. Read before the mutable calls below, because they borrow
        // `self` exclusively.
        let declares_module = self.spec.is_module_node(node.kind());
        let fingerprint = match declares_module {
            true => self.text(node).map(|body| modules::fingerprint(&body)),
            false => None,
        };

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
            structural_fingerprint: fingerprint,
        };
        self.entities.push(entity);

        // Containment is expressed as relations rather than a side table, so that every edge in
        // the graph has exactly one representation. A module declared at file level has no
        // enclosing *declaration*, so it hangs off the module this file is rather than off the
        // file: the two are the same namespace seen from two directions, and putting the module
        // outside the file would leave the package's namespace unrooted.
        let parent = match self.scope.last() {
            Some(enclosing) => Some(enclosing.id.clone()),
            None if declares_module => self.module_id.clone(),
            None => None,
        };
        if let Some(parent) = parent {
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
        let end = self.source.text().get(parameters.byte_range())?.find(')')?
            + parameters.start_byte()
            - node.start_byte()
            + 1;
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
        let subject = self
            .current_entity()
            .cloned()
            .unwrap_or_else(|| self.file_id.clone());

        if let Some(rule) = self.spec.call_rule(node.kind())
            && let Some(callee_node) = node.child_by_field_name(rule.callee_field)
        {
            let callee = self.callee(callee_node);
            self.emit_call(subject.clone(), node, &callee);
        }

        if let Some(rule) = self.spec.import_rule(node.kind()) {
            self.emit_import(subject.clone(), node, rule);
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
                format!("call to `{}`", callee.name),
            ));
        }
    }

    /// Emit `imports` relations from a `use` declaration, preserving aliases.
    fn emit_import(&mut self, source: EntityId, node: Node<'_>, rule: &ImportRule) {
        let Some(argument) = rule
            .path_field
            .and_then(|field| node.child_by_field_name(field))
        else {
            return;
        };
        // `pub use` is still a binding in this module, so it is still an `Imports` edge and the
        // resolver's R1 rung still places names against it. What the marker buys is the word in
        // the basis, so `peek explain` can say the name is part of the module's public surface
        // without re-parsing the file. It does *not* become a `Reexports` edge: that kind exists
        // and is never emitted, because a second edge for the same binding would be decided by
        // the same rung and land in the unresolved bucket twice, inflating the number the whole
        // project is trying to bring down.
        let reexport = self.has_child_of_kind(node, rule.reexport_markers);
        // Copy the text out so the binding walk does not borrow `self` while we push relations.
        let text = self.source.text().to_owned();
        let span = self.span(node);
        let mut bindings = Vec::new();
        collect_use_bindings(text.as_str(), argument, "", &mut bindings);
        for binding in bindings {
            let imported = binding.local.clone();
            let basis = match reexport {
                true => format!("re-export of `{imported}` from `{}`", binding.module),
                false => format!("import of `{imported}`"),
            };
            self.relations.push(Relation::pending(
                RelationKind::Imports,
                source.clone(),
                binding.local,
                span,
                Evidence::ImportBinding {
                    module: binding.module,
                    alias: binding.alias,
                },
                basis,
            ));
        }
    }

    /// Whether any direct child of `node` has one of the given kinds.
    fn has_child_of_kind(&self, node: Node<'_>, kinds: &[&str]) -> bool {
        if kinds.is_empty() {
            return false;
        }
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .any(|child| kinds.contains(&child.kind()))
    }

    /// Emit `implements` and `inherits` relations.
    fn emit_inheritance(&mut self, node: Node<'_>, style: InheritanceStyle) {
        match style {
            InheritanceStyle::TraitBounds {
                trait_decl_node,
                bounds_field,
                impl_node,
                impl_trait_field,
                impl_type_field,
                ..
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
                        (name, node.child_by_field_name(bounds_field))
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
    ///
    /// The state is [`ResolutionState::Pending`], not [`ResolutionState::Inferred`]. `Inferred`
    /// means the target is known *by inference*, and it therefore still has one — the difference
    /// is how we know, not whether we know. A clause naming a type this file does not declare has
    /// no target at all yet, which is exactly what `Pending` means. Emitting `Inferred` here
    /// produced a relation that claimed a resolution it did not have, and the store's own CHECK
    /// constraint rejected the extractor's normal output — which is the constraint working.
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
        self.relations.push(Relation::pending(
            kind,
            anchor,
            bound,
            span,
            Evidence::NameOnly,
            format!("{basis} relation on `{subject}`"),
        ));
    }

    fn emit_reference(&mut self, source: EntityId, node: Node<'_>) {
        if let Some(name) = self.text(node)
            && !name.is_empty()
        {
            // `name` is moved into the relation, so the basis is built first.
            let basis = format!("reference to `{name}`");
            self.relations.push(Relation::pending(
                RelationKind::References,
                source,
                name,
                self.span(node),
                Evidence::NameOnly,
                basis,
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
        source
            .get(node.byte_range())
            .map(str::trim)
            .map(str::to_owned)
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
        // `use a::*;` — the path is an unnamed child, not a field, so the text of the node is
        // the whole `a::*`. The module a glob names is the prefix and the `*` is the local name;
        // carrying the star into the path would hand the resolver a module called `a::*`, which
        // is a path no `use` statement can ever spell. Inside a group — `use a::{b::*, c};` —
        // the enclosing `scoped_use_list` has already put `a` in `prefix`, and the two join.
        "use_wildcard" => {
            let base = text_of(node).unwrap_or_default();
            let stem = base.strip_suffix("::*").unwrap_or(&base).trim();
            if !stem.is_empty() {
                out.push(UseBinding {
                    local: "*".to_owned(),
                    module: join_path(prefix, stem),
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
    path.rsplit("::")
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
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
            module_ids: Vec::new(),
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
            module_ids: Vec::new(),
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
        module_ids: walker.module_ids,
        degradation: (errors > 0 || missing > 0).then_some(Degradation {
            error_nodes: errors,
            missing_nodes: missing,
            first_error_byte: first_error.map(|byte| byte.min(u32::MAX as usize) as u32),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtractedFile, extract_with};
    use crate::extract::registry;
    use crate::model::{EntityKind, RelationKind, RepoPath, ResolutionState};

    fn path() -> RepoPath {
        RepoPath::new("src/payments.rs").expect("valid path")
    }

    fn rust(source: &str) -> ExtractedFile {
        extract_with(
            registry::get(crate::model::Language::Rust).expect("rust spec"),
            path(),
            source,
        )
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

    /// Extract at an arbitrary repository-relative path, which is the only thing that decides a
    /// module's qualified name.
    fn rust_at(path: &str, source: &str) -> ExtractedFile {
        extract_with(
            registry::get(crate::model::Language::Rust).expect("rust spec"),
            RepoPath::new(path).unwrap_or_else(|| panic!("{path} is not a valid path")),
            source,
        )
    }

    /// The module and package entities this extraction contributed, as qualified names.
    fn module_names(file: &ExtractedFile) -> Vec<String> {
        let mut names: Vec<String> = file
            .entities
            .iter()
            .filter(|entity| file.module_ids.contains(&entity.id))
            .map(|entity| format!("{} {}", entity.kind().as_str(), entity.id.qualified_name()))
            .collect();
        names.sort();
        names
    }

    /// The `module` recorded in every `ImportBinding` on the given relations, in order.
    fn import_modules(file: &ExtractedFile) -> Vec<String> {
        let mut modules = Vec::new();
        for relation in &file.relations {
            if relation.kind != RelationKind::Imports {
                continue;
            }
            match &relation.resolution {
                ResolutionState::Pending {
                    evidence: crate::model::Evidence::ImportBinding { module, .. },
                    ..
                } => modules.push(module.clone()),
                other => panic!(
                    "an import must carry its binding, got {other:?} for `{}`",
                    relation.target_name
                ),
            }
        }
        modules
    }

    /// The one import edge in a file that has exactly one, so a test can look at its evidence.
    fn only_import(file: &ExtractedFile) -> &crate::model::Relation {
        let imports: Vec<&crate::model::Relation> = file
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Imports)
            .collect();
        assert_eq!(
            imports.len(),
            1,
            "this fixture has exactly one import, and the relations were {:?}",
            file.relations
                .iter()
                .map(|relation| format!("{} `{}`", relation.kind, relation.target_name))
                .collect::<Vec<_>>()
        );
        imports[0]
    }

    /// A throwaway index directory that removes itself, so a test that needs one leaves nothing
    /// behind for the next and a failing test does not strand a database in the temp folder.
    struct ScratchIndex(std::path::PathBuf);

    impl ScratchIndex {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "peek-extract-{label}-{}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the scratch directory");
            Self(path)
        }

        fn open(&self) -> crate::store::Store {
            let repo =
                crate::store::RepoId::discover(&self.0).expect("derive a repository identity");
            crate::store::Store::open(&self.0.join("index.db"), &repo).expect("open the store")
        }
    }

    impl Drop for ScratchIndex {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
        assert!(
            names.contains(&"PaymentService.new".to_owned()),
            "{names:?}"
        );
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
        let file = rust("trait Base {} trait Extended: Base + Send {}");
        let inherits: Vec<&str> = file
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Inherits)
            .map(|r| r.target_name.as_str())
            .collect();
        assert!(inherits.contains(&"Base"), "inherits was {inherits:?}");
        assert!(inherits.contains(&"Send"), "inherits was {inherits:?}");
    }

    #[test]
    fn supertraits_emit_inherits_from_a_multiline_file() {
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
        assert!(inherits.contains(&"Base"), "inherits was {inherits:?}");
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
        // `impl dyn Trait {}` has no `impl_item.trait` field at all — the bound lives at
        // `type` -> `dynamic_type` -> `trait`. Reading only `impl_item.trait` loses it silently.
        // (Confirmed against the real grammar: `impl dyn X for Y` is not even valid Rust and
        // parses to an ERROR node, so the bound is only reachable in the marker form.)
        let file = rust("impl dyn std::fmt::Debug {}");
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
            ResolutionState::Pending { evidence, .. } => match evidence {
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
        assert_eq!(
            call.resolution.evidence_class(),
            Some("qualified_name_in_scope")
        );
    }

    #[test]
    fn macro_invocations_are_calls() {
        // `println!` is a `macro_invocation`, never a `call_expression`, and a Rust codebase is
        // full of them. Omitting them would badly understate call counts.
        let file = rust("fn main() { println!(\"hi\"); }");
        assert!(
            relation_count(&file, RelationKind::Calls) >= 1,
            "macro call missing"
        );
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
            ResolutionState::Pending { evidence, .. } => match evidence {
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
        let method = file
            .entities
            .iter()
            .find(|entity| entity.id.qualified_name() == "S.m")
            .expect("the method was extracted");
        let edge = file
            .relations
            .iter()
            .find(|relation| {
                relation.kind == RelationKind::Contains
                    && relation.target.as_ref() == Some(&method.id)
            })
            .expect("the method is contained in the impl block, not in a side table");
        assert_eq!(edge.source.qualified_name(), "S");
        assert_eq!(
            edge.resolution,
            ResolutionState::Resolved {
                by: crate::model::Evidence::Containment
            }
        );
    }

    #[test]
    fn summary_helpers_count_by_kind() {
        let file = rust("struct S; fn f() { g(); } fn g() {}");
        let entities = file.entity_counts();
        assert_eq!(entities.get(&EntityKind::Function), Some(&2));
        assert_eq!(entities.get(&EntityKind::Struct), Some(&1));
        assert_eq!(entities.get(&EntityKind::File), Some(&1));

        let relations = file.relation_counts();
        assert!(
            relations.contains_key(&RelationKind::Calls),
            "{relations:?}"
        );
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
    fn unknown_language_yields_none_not_an_empty_file() {
        // Conflating "no rules" with "no symbols" is how Cortex reported 14 dead languages as
        // supported.
        let markdown = RepoPath::new("README.md").expect("valid path");
        assert!(super::extract(markdown, "# hi").is_none());
    }

    // -----------------------------------------------------------------------
    // Modules
    //
    // A module is a declaration, not a reference. Everything below is about the index gaining a
    // module table without any symbol changing identity, any call changing target, and any
    // module name out-ranking a real one.
    // -----------------------------------------------------------------------

    #[test]
    fn a_file_is_the_module_its_path_names() {
        // The claim R-007 rests on. `use regex_automata::util::look::Matcher;` can only be a
        // lookup if the index holds a module spelled `regex_automata::util::look`.
        let file = rust_at(
            "crates/regex-automata/src/util/look.rs",
            "pub struct Matcher;",
        );
        assert_eq!(
            module_names(&file),
            vec!["module regex_automata::util::look".to_owned()]
        );
    }

    #[test]
    fn a_mod_rs_file_is_the_module_its_directory_names_and_the_file_below_it_is_its_child() {
        let directory = rust_at("crates/foo/src/util/mod.rs", "pub mod inner;");
        let child = rust_at("crates/foo/src/util/look.rs", "pub struct Matcher;");
        assert_eq!(
            module_names(&directory),
            vec!["module foo::util".to_owned()],
            "the `mod` in `mod.rs` is a filename convention, not part of the name"
        );
        assert_eq!(
            module_names(&child),
            vec!["module foo::util::look".to_owned()],
            "a file inside a module directory is that module's child, not a sibling"
        );
    }

    #[test]
    fn a_crate_root_file_names_its_package_and_only_the_crate_root_does() {
        let root = rust_at("crates/regex/src/lib.rs", "pub mod automata;");
        let member = rust_at("crates/regex/src/automata/mod.rs", "pub struct Util;");
        assert_eq!(
            module_names(&root),
            vec!["module regex".to_owned(), "package regex".to_owned(),],
            "the crate root is the one file that declares the package"
        );
        assert_eq!(
            module_names(&member),
            vec!["module regex::automata".to_owned()],
            "a module inside a package does not declare a second package"
        );
    }

    #[test]
    fn a_module_declaration_keeps_the_qualified_name_it_always_had() {
        // The compromise that makes this change safe. `mod inner` is the module `inner`, inside
        // the file that is the module `foo::app`. Prefixing it would change the qualified name —
        // and therefore the `EntityId` — of every symbol inside it, so a refactor that moved
        // nothing would re-key every method in the repository.
        let file = rust_at(
            "crates/foo/src/app.rs",
            "pub mod inner { pub fn helper() {} }",
        );
        let names = qualified_names(&file);
        assert!(
            names.contains(&"inner".to_owned()),
            "the inline module keeps its own name: {names:?}"
        );
        assert!(
            names.contains(&"inner.helper".to_owned()),
            "the function inside it keeps its qualified name: {names:?}"
        );
        assert!(
            !names.contains(&"foo::app::inner.helper".to_owned()),
            "and it is *not* re-keyed into the module path: {names:?}"
        );
    }

    #[test]
    fn a_module_declaration_is_contained_by_the_module_the_file_is() {
        let file = rust_at("crates/foo/src/app.rs", "pub mod inner;");
        let module = file
            .entities
            .iter()
            .find(|entity| file.module_ids.contains(&entity.id))
            .expect("the file is a module");
        let inner = file
            .entities
            .iter()
            .find(|entity| entity.name == "inner" && !file.module_ids.contains(&entity.id))
            .expect("the declaration was extracted");
        let edge = file
            .relations
            .iter()
            .find(|relation| relation.target.as_ref() == Some(&inner.id))
            .expect("the declaration is contained in the module that declares it");
        assert_eq!(edge.kind, RelationKind::Contains);
        assert_eq!(edge.source, module.id);
        assert_eq!(
            edge.resolution,
            ResolutionState::Resolved {
                by: crate::model::Evidence::Containment
            }
        );
    }

    #[test]
    fn a_declaration_inside_a_module_is_contained_by_the_module_not_by_the_file() {
        // A `use` at file level has always hung off the file entity, and it still does: moving it
        // to the module would change the source of every existing import edge for no gain the
        // resolver could use.
        let file = rust_at(
            "crates/foo/src/app.rs",
            "pub mod inner { use crate::payments::Service; }",
        );
        let inner = file
            .entities
            .iter()
            .find(|entity| entity.name == "inner" && !file.module_ids.contains(&entity.id))
            .expect("the module was extracted");
        let import = file
            .relations
            .iter()
            .find(|relation| relation.kind == RelationKind::Imports)
            .expect("the import survived");
        assert_eq!(
            import.source, inner.id,
            "an import inside a module is scoped to that module"
        );
    }

    #[test]
    fn a_module_does_not_add_a_symbol_a_method_or_a_call_to_what_a_file_already_extracted() {
        // The before/after proof. The expected values are what the walker on `origin/main`
        // produced for this source, transcribed from the algorithm that produced them, so a
        // change to any existing extraction shows up here as a diff rather than as an index that
        // quietly differs.
        let file = rust_at(
            "crates/foo/src/app.rs",
            r#"
            use std::collections::HashMap;
            use crate::payments::Service as Svc;

            pub mod payments {
                pub struct Service;
                impl Service {
                    pub fn new() -> Service { Service }
                }
            }
            fn main() { payments::Service::new(); }
            "#,
        );
        assert!(file.is_clean(), "{:?}", file.degradation);

        let mut declarations: Vec<String> = file
            .entities
            .iter()
            .filter(|entity| !file.module_ids.contains(&entity.id))
            .map(|entity| {
                format!(
                    "{} {}#{}",
                    entity.kind().as_str(),
                    entity.id.qualified_name(),
                    entity.id.ordinal()
                )
            })
            .collect();
        declarations.sort();
        assert_eq!(
            declarations,
            vec![
                "file app.rs#0".to_owned(),
                "function main#0".to_owned(),
                "method payments.Service.new#0".to_owned(),
                "module payments#0".to_owned(),
                "module payments.Service#0".to_owned(),
                "struct payments.Service#0".to_owned(),
            ],
            "every entity, kind, qualified name and ordinal the walker produced before modules \
             existed"
        );

        // An edge is new exactly when it touches a module entity this change introduced. Naming
        // them is the point: a count would hide a `Contains` edge that quietly re-parented
        // something that used to hang off the file.
        let describe = |relation: &crate::model::Relation| {
            let target = match relation.target.as_ref() {
                Some(id) => format!("{} {}", id.kind().as_str(), id.qualified_name()),
                // An import or a call has no target yet: the resolver decides it later, and
                // until then the name as written is the whole of what is known.
                None => relation.target_name.clone(),
            };
            format!(
                "{} {} -> {}",
                relation.kind.as_str(),
                relation.source.qualified_name(),
                target
            )
        };
        let existing: Vec<String> = file
            .relations
            .iter()
            .filter(|relation| {
                !file.module_ids.contains(&relation.source)
                    && !relation
                        .target
                        .as_ref()
                        .is_some_and(|target| file.module_ids.contains(target))
            })
            .map(describe)
            .collect();
        assert_eq!(
            existing,
            vec![
                "imports app.rs -> HashMap".to_owned(),
                "imports app.rs -> Svc".to_owned(),
                "contains payments -> struct payments.Service".to_owned(),
                "contains payments -> module payments.Service".to_owned(),
                "contains payments.Service -> method payments.Service.new".to_owned(),
                "calls main -> new".to_owned(),
            ],
            "every relation the walker produced before modules existed, in source order"
        );

        let added: Vec<String> = file
            .relations
            .iter()
            .filter(|relation| {
                file.module_ids.contains(&relation.source)
                    || relation
                        .target
                        .as_ref()
                        .is_some_and(|target| file.module_ids.contains(target))
            })
            .map(describe)
            .collect();
        assert_eq!(
            added,
            vec![
                "contains app.rs -> module foo::app".to_owned(),
                "contains foo::app -> module payments".to_owned(),
            ],
            "the whole of the difference: the file contains the module it is, and that module \
             contains the module the file declares"
        );
    }

    #[test]
    fn a_module_and_a_symbol_of_the_same_name_are_both_in_the_index_and_neither_replaced() {
        // Audit B21 is what happens when a *file* beats a symbol: a well-formed empty answer
        // instead of a reported ambiguity. A module is the same hazard one step along, so the
        // first half of the answer is that the module and the symbol are different entities with
        // different identities, and emitting one did not displace the other.
        let file = rust_at("crates/foo/src/foo.rs", "pub fn foo() {}");
        let module = file
            .entities
            .iter()
            .find(|entity| file.module_ids.contains(&entity.id))
            .expect("the file is a module");
        let function = file
            .entities
            .iter()
            .find(|entity| entity.kind() == EntityKind::Function)
            .expect("the function was extracted");
        assert_eq!(module.name, "foo");
        assert_eq!(function.name, "foo");
        assert_eq!(module.id.qualified_name(), "foo::foo");
        assert_ne!(module.id, function.id);
        assert!(
            file.module_ids.contains(&module.id) && !file.module_ids.contains(&function.id),
            "the module is in the module table and the function is not, which is what stops a \
             consumer from treating every namespace as a declaration"
        );
    }

    #[test]
    fn a_module_never_wins_a_name_lookup_against_a_real_symbol() {
        // The end-to-end half, through the store and the resolver rather than through a struct
        // field. Two files: one declares a module and a function of the same name, the other
        // calls that name with nothing in scope, which is the only route left for a name lookup.
        //
        // The property asserted is deliberately the *property*, not today's exact answer. Today
        // the ladder finds two candidates and returns `Ambiguous`; if the resolver is later taught
        // that a module is not a target of a bare name, the honest answer becomes `Inferred` on
        // the function, and this test must keep passing. What it refuses to accept is a call that
        // resolved to the module.
        let declaring = rust_at("crates/foo/src/foo.rs", "pub fn foo() {}");
        let calling = rust_at("crates/foo/src/caller.rs", "fn main() { foo(); }");
        let module = declaring
            .entities
            .iter()
            .find(|entity| declaring.module_ids.contains(&entity.id))
            .expect("the file is a module")
            .id
            .clone();
        let function = declaring
            .entities
            .iter()
            .find(|entity| entity.kind() == EntityKind::Function)
            .expect("the function was extracted")
            .id
            .clone();
        let caller = calling
            .entities
            .iter()
            .find(|entity| entity.kind() == EntityKind::Function)
            .expect("the caller was extracted")
            .id
            .clone();

        let scratch = ScratchIndex::new("module-lookup");
        let mut store = scratch.open();
        let mut update = crate::store::IndexUpdate::empty();
        for entity in declaring.entities.iter().chain(calling.entities.iter()) {
            update = update.with_entity(entity.clone());
        }
        for relation in declaring.relations.iter().chain(calling.relations.iter()) {
            update = update.with_relation(relation.clone());
        }
        store
            .apply_update(update)
            .expect("the extraction is storable");
        let options = crate::resolve::ResolutionOptions::default();
        let report = crate::resolve::resolve_all(&mut store, options).expect("the pass runs");
        assert!(!report.pending_remaining, "{report:?}");

        let call = store
            .outgoing(&caller, Some(RelationKind::Calls), 16)
            .expect("the call is readable")
            .into_iter()
            .find(|relation| relation.target_name == "foo")
            .expect("the call to `foo` survived the round trip");
        match &call.resolution {
            ResolutionState::Ambiguous { candidates } => {
                assert!(
                    candidates.contains(&function) && candidates.contains(&module),
                    "both candidates are written down and neither is a recommendation: \
                     {candidates:?}"
                );
            }
            other => {
                assert_eq!(
                    call.target.as_ref(),
                    Some(&function),
                    "the call was decided to something other than ambiguity and it must be the \
                     function, never the module; it was {other:?}"
                );
            }
        }
        assert_ne!(
            call.target.as_ref(),
            Some(&module),
            "a module is a declaration, not something a bare call can bind to: the call resolved \
             to {:?} with state {}",
            call.target,
            call.resolution.describe()
        );
    }

    // -----------------------------------------------------------------------
    // Imports
    // -----------------------------------------------------------------------

    #[test]
    fn an_import_carries_the_whole_path_it_was_written_with() {
        // `use a::b::C;` collapsing to `C` is the exact defect this rules out: the module path
        // in the evidence is what the resolver's R1 rung needs and there is nowhere else to get
        // it from.
        let file = rust_at(
            "crates/foo/src/app.rs",
            r#"
            use crate::a::B;
            use super::C;
            use self::D;
            use other_crate::E;
            "#,
        );
        assert_eq!(
            import_modules(&file),
            vec![
                "crate::a::B".to_owned(),
                "super::C".to_owned(),
                "self::D".to_owned(),
                "other_crate::E".to_owned(),
            ]
        );
    }

    #[test]
    fn an_import_of_a_named_item_keeps_the_module_and_the_item_apart() {
        // `use a::b::C` may mean the item `C` in module `a::b` or the module `a::b::C`, and
        // nothing in the syntax says which. The resolver tries both readings, so the evidence has
        // to carry the whole path and not a pre-judgement about which it is.
        let file = rust_at("crates/foo/src/app.rs", "use crate::a::b::C;");
        assert_eq!(import_modules(&file), vec!["crate::a::b::C".to_owned()]);
        let import = file
            .relations
            .iter()
            .find(|relation| relation.kind == RelationKind::Imports)
            .expect("the import survived");
        assert_eq!(
            import.target_name, "C",
            "the local name is the last segment"
        );
    }

    #[test]
    fn an_alias_carries_the_path_it_renamed_not_just_the_new_name() {
        let file = rust_at("crates/foo/src/app.rs", "use crate::a::b::C as Renamed;");
        assert_eq!(import_modules(&file), vec!["crate::a::b::C".to_owned()]);
        match &only_import(&file).resolution {
            ResolutionState::Pending {
                evidence: crate::model::Evidence::ImportBinding { alias, .. },
                ..
            } => assert_eq!(alias.as_deref(), Some("Renamed")),
            other => panic!("expected an import binding, got {other:?}"),
        }
    }

    #[test]
    fn a_group_import_gives_every_name_the_full_path_to_it() {
        let file = rust_at("crates/foo/src/app.rs", "use crate::a::{B, b::C as D};");
        assert_eq!(
            import_modules(&file),
            vec!["crate::a::B".to_owned(), "crate::a::b::C".to_owned()]
        );
    }

    #[test]
    fn a_glob_import_binds_the_star_and_names_the_module_it_opens() {
        // The star is the local name and the prefix is the path. A glob is not a name anything
        // can be resolved against — the resolver's first step is to refuse it as `Unsupported` —
        // so the only thing that has to be right is that neither half is mistaken for the other.
        let file = rust_at(
            "crates/foo/src/app.rs",
            r#"
            use crate::a::b::*;
            use crate::c::{d::*, e};
            "#,
        );
        let locals: Vec<&str> = file
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Imports)
            .map(|relation| relation.target_name.as_str())
            .collect();
        assert_eq!(
            locals,
            vec!["*", "*", "e"],
            "a star is a binding, not a name"
        );
        assert_eq!(
            import_modules(&file),
            vec![
                "crate::a::b".to_owned(),
                "crate::c::d".to_owned(),
                "crate::c::e".to_owned(),
            ]
        );
    }

    #[test]
    fn a_public_use_is_a_binding_whose_basis_says_it_is_part_of_the_public_surface() {
        // It stays an `Imports` edge, because the resolver's R1 rung reads only `Imports` and a
        // re-export is a name every other file in the module is resolved against. Making it a
        // second, differently-typed edge would double the unresolved bucket without adding a
        // single placement.
        let file = rust_at("crates/foo/src/lib.rs", "pub use crate::a::B;");
        assert_eq!(relation_count(&file, RelationKind::Imports), 1);
        assert_eq!(
            relation_count(&file, RelationKind::Reexports),
            0,
            "one binding is one edge; a second edge for the same binding would be decided by the \
             same rung and land in the unresolved bucket twice"
        );
        match &only_import(&file).resolution {
            ResolutionState::Pending { basis, .. } => assert!(
                basis.contains("re-export") && basis.contains("crate::a::B"),
                "the basis has to say both what was re-exported and from where: {basis:?}"
            ),
            other => panic!("expected a pending import, got {other:?}"),
        }
    }

    #[test]
    fn a_private_use_is_a_binding_and_does_not_claim_to_be_a_re_export() {
        let file = rust_at("crates/foo/src/lib.rs", "use crate::a::B;");
        match &only_import(&file).resolution {
            ResolutionState::Pending { basis, .. } => {
                assert!(!basis.contains("re-export"), "{basis:?}")
            }
            other => panic!("expected a pending import, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // The committed import trees
    // -----------------------------------------------------------------------

    /// Extract a committed fixture through the same path the indexer would use.
    fn fixture(relative: &str) -> ExtractedFile {
        rust_at(relative, &crate::extract::modules::fixture_source(relative))
    }

    #[test]
    fn a_cross_package_import_names_a_module_in_the_other_package() {
        // The measured problem. `use alpha::gateway::Gateway;` in `beta` cannot be placed without
        // a module table, and with one it is a name the index already holds: `alpha::gateway`.
        let service = fixture("cross/beta/src/service.rs");
        assert_eq!(
            import_modules(&service),
            vec!["alpha::gateway::Gateway".to_owned()],
            "the path keeps the package prefix, so the first segment is another crate rather than \
             a module of this one"
        );
        let reexport = fixture("cross/beta/src/lib.rs");
        assert_eq!(
            import_modules(&reexport),
            vec!["alpha::gateway::Gateway".to_owned()],
            "a re-export across packages carries the same complete path"
        );
    }

    #[test]
    fn a_crate_internal_import_names_a_module_in_the_same_package() {
        let service = fixture("several/src/b.rs");
        assert_eq!(import_modules(&service), vec!["crate::a::Alpha".to_owned()]);
    }

    #[test]
    fn a_renamed_re_export_binds_the_new_name_and_remembers_the_old_path() {
        let lib = fixture("reexport/src/lib.rs");
        let renamed = lib
            .relations
            .iter()
            .find(|relation| relation.target_name == "Renamed")
            .expect("the re-export bound `Renamed`");
        match &renamed.resolution {
            ResolutionState::Pending {
                evidence: crate::model::Evidence::ImportBinding { module, alias },
                basis,
            } => {
                assert_eq!(
                    module, "crate::inner::Thing",
                    "the path is not lost to the alias"
                );
                assert_eq!(alias.as_deref(), Some("Renamed"));
                assert!(basis.contains("re-export"), "{basis:?}");
            }
            other => panic!("expected an import binding, got {other:?}"),
        }
    }

    #[test]
    fn a_glob_re_export_opens_a_module_without_binding_any_of_the_names_inside_it() {
        // The temptation here is to bind `assist` because the file that defines it is in the
        // same crate. Nothing in the syntax says which names a glob brings in, so binding one
        // would be a guess about the author's intent, and the star is the only thing the
        // declaration actually says.
        let lib = fixture("reexport/src/lib.rs");
        let targets: Vec<&str> = lib
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Imports)
            .map(|relation| relation.target_name.as_str())
            .collect();
        assert_eq!(targets, vec!["Renamed", "*"], "{targets:?}");
        assert_eq!(
            import_modules(&lib),
            vec![
                "crate::inner::Thing".to_owned(),
                "crate::inner::helpers".to_owned(),
            ],
            "the glob names the module it opens, with the star stripped off"
        );
    }

    #[test]
    fn a_module_in_a_reexport_tree_is_not_reachable_through_the_star_that_opens_it() {
        // `assist` exists in `reexport::inner::helpers` and `*` opens that module. Asserting
        // that no edge claims to bind `assist` is the negative half of the previous test, and it
        // is the half that would catch a glob quietly turning into a name lookup — which is
        // audit B6, where `from .utils import helper` became a repo-global lookup on `helper`.
        let lib = fixture("reexport/src/lib.rs");
        let invented: Vec<&str> = lib
            .relations
            .iter()
            .filter(|relation| relation.target_name == "assist")
            .map(|relation| relation.kind.as_str())
            .collect();
        assert!(
            invented.is_empty(),
            "a glob must not manufacture a binding for a name it never wrote: {invented:?}"
        );
    }
}
