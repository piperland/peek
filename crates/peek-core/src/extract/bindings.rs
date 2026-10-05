//! Classifying a name that a binder in the source's own scope has already claimed.
//!
//! # The defect this exists to stop
//!
//! `let mut out = ...; out.push_str(..)` binds `out` in `summarise`. The extractor emits no
//! entity for a `let` binding — the name is an `identifier`, and `identifier` is not in any
//! spec's `symbols` — so nothing in the graph can name it. The reference is nevertheless a
//! relation, and the resolver placed it on a *different* entity that happens to share the
//! name: `report.rs`'s `render.out` parameter. The edge was decided and wrong.
//!
//! So the extractor has to say something about it that the resolver can act on, and it can
//! only say it from the tree: the walker is the one place that still knows which binder is in
//! force at which byte.
//!
//! # What this module returns
//!
//! The **node type of the binder that makes the occurrence local**, or `None` when nothing
//! reachable binds the name. `None` is not "not a local": it is "this classifier says
//! nothing", and the two are kept apart because only the second would be a claim.
//!
//! # Three clauses, and each was found by measurement rather than by argument
//!
//! 1. **A member name is not classified by its receiver's binder.** `render` reads
//!    `sink.text`; the local is `sink` and the name `text` resolves to `Sink.text` today.
//!    Classifying it by the binder of the receiver replaces a correct edge with a gap.
//! 2. **A name such a binder binds is local.** `let mut out = ..` then `out.push_str(..)`.
//!    The binder is a **sibling** of the use, not an ancestor — walking ancestors finds
//!    nothing at all, which classifies nothing and reads as a rule that does no harm.
//! 3. **A name such a binder introduces is local.** `out` in `let mut out = ..` is the
//!    declaration itself. Ten of the fixture's `binds_nothing` rows are of this shape, one
//!    per local binding written, and a rule with only clause 2 leaves every one of them
//!    decided and wrong.
//!
//! # What is not covered, stated rather than hidden
//!
//! * **A `for` loop written as a statement is an `expression_statement` around a
//!   `for_expression`.** Looking for a binder among a block's direct children finds
//!   nothing. One level of unwrapping is all that is done here; a binder nested deeper is not
//!   a shape that has been measured.
//! * **A field shorthand has no field name.** `Entry { count }` is a
//!   `shorthand_field_initializer` holding a bare `identifier`, so clause 1 cannot see it and
//!   the occurrence is classified by whatever binders are in scope. On the fixture no binder
//!   is in scope there, so it resolves to the local it reads — which is right — but a `count`
//!   shorthand under a `let count` would be called local. That is the shape a field name and
//!   a value read share in one occurrence, and the relation model has one name per row.
//! * **`if let`, `while let` and match-arm patterns are absent from
//!   [`crate::extract::spec::LanguageSpec::bindings`].** They bind names too. Absent from the
//!   table means unclassified, which is not the same as not local.

use tree_sitter::Node;

use super::spec::LanguageSpec;

/// The node type of a binder that makes `node` local, or `None` when none is in reach.
///
/// The occurrence must be one the spec's reference rule already recognises: this reads a
/// single identifier occurrence and says nothing about whether it is a *use* (that is
/// [`crate::extract::walker`]'s own-name test, which runs first).
///
/// `source` is the whole file. Nothing is read from it beyond the bytes of the identifier
/// nodes themselves, so a name inside a comment or a string literal cannot satisfy a match.
pub fn local_binding(spec: &LanguageSpec, node: Node<'_>, source: &[u8]) -> Option<&'static str> {
    let name = std::str::from_utf8(source.get(node.byte_range())?).ok()?;
    if name.is_empty() {
        return None;
    }

    // Clause 1. Asked first because it is the only clause that can answer for an occurrence
    // whose *parent* already answers it: the field of `sink.text` names a member of
    // whatever `sink` holds, and no binder that binds `sink` has anything to say about it.
    if names_a_member(spec, node) {
        return None;
    }

    // Clause 3 before clause 2, because an occurrence the binder *writes* is also inside the
    // binder, and both answers are the same string — the order only fixes which check is
    // asked first, and clause 3 is the cheaper of the two.
    introduces(spec, node, source, name).or_else(|| bound_around(spec, node, source, name))
}

/// Whether this occurrence sits in a slot its spec calls a member's.
///
/// The question is asked of the **parent**, because a slot is a parent's field: `field` is a
/// field of `field_expression` and of `field_initializer`, and the two node types are not
/// named here.
fn names_a_member(spec: &LanguageSpec, node: Node<'_>) -> bool {
    let Some(member_fields) = spec.references.as_ref().map(|rule| rule.member_fields) else {
        return false;
    };
    if member_fields.is_empty() {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    let mut cursor = parent.walk();
    for (index, child) in parent.named_children(&mut cursor).enumerate() {
        if child.id() == node.id() {
            return parent
                .field_name_for_named_child(index as u32)
                .is_some_and(|field| member_fields.contains(&field));
        }
    }
    false
}

/// Whether a binder encloses this occurrence **and writes its name** — clause 3.
fn introduces(
    spec: &LanguageSpec,
    node: Node<'_>,
    source: &[u8],
    name: &str,
) -> Option<&'static str> {
    let mut current = node.parent();
    while let Some(binder) = current {
        if let Some(rule) = spec.binding_rule(binder.kind())
            && let Some(names) = binder.child_by_field_name(rule.name_field)
            && covers(&names, node)
            && binds(spec, &names, source, name)
        {
            return Some(rule.node_type);
        }
        current = binder.parent();
    }
    None
}

/// Whether a binder **around** this occurrence binds its name — clause 2.
///
/// # A binder is a sibling, not an ancestor
///
/// `let mut out = ...;` and `out.push_str(..)` are two statements of one block. The walk is
/// therefore over each enclosing node's own children, not over the ancestors: an ancestor
/// walk finds nothing, which is not a subtle miss but a classifier that never fires.
///
/// Three filters, and each is a mistake that has been made:
///
/// * **Only children that begin before the occurrence.** `let x = ..;` names `x` for what
///   comes after it, and an occurrence at or before the end of the pattern is not bound by
///   it.
/// * **Only what the grammar says is not yet in force is excluded.** `let size = size + 1`
///   reads the `size` that existed before the statement, so the initialiser's own occurrence
///   is not bound by the declaration even though it is textually after the pattern. The
///   field that says so is per node type ([`crate::extract::spec::BindingRule`]), and this
///   reads it rather than assuming one rule covers the three.
/// * **One level of unwrapping.** A `for` loop written as a statement is an
///   `expression_statement` around a `for_expression`.
fn bound_around(
    spec: &LanguageSpec,
    node: Node<'_>,
    source: &[u8],
    name: &str,
) -> Option<&'static str> {
    let mut current = node.parent();
    while let Some(enclosing) = current {
        let mut cursor = enclosing.walk();
        for sibling in enclosing.named_children(&mut cursor) {
            if sibling.start_byte() >= node.start_byte() {
                break;
            }
            if let Some((rule, binder)) = as_binder(spec, sibling)
                && binder
                    .child_by_field_name(rule.name_field)
                    .is_some_and(|names| {
                        in_force(rule.not_in_force, &binder, &names, node)
                            && binds(spec, &names, source, name)
                    })
            {
                return Some(rule.node_type);
            }
        }
        current = enclosing.parent();
    }
    None
}

/// A node as the binder it is, or the binder it wraps in its one wrapper node.
///
/// Returns the rule with it, because the rule is what says where the names are and where the
/// binding is not yet in force, and looking those up again at the use site would be two
/// places to forget them.
fn as_binder<'t>(
    spec: &LanguageSpec,
    node: Node<'t>,
) -> Option<(&'static super::spec::BindingRule, Node<'t>)> {
    if let Some(rule) = spec.binding_rule(node.kind()) {
        return Some((rule, node));
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter_map(|child| spec.binding_rule(child.kind()).map(|rule| (rule, child)))
        .next()
}

/// Whether the binder is in force at `node`.
///
/// Forward first: an occurrence at or before the end of the pattern is the pattern's own, or
/// an earlier statement's, and is not bound by this binder.
fn in_force(
    not_in_force: Option<&'static str>,
    binder: &Node<'_>,
    names: &Node<'_>,
    node: Node<'_>,
) -> bool {
    if node.start_byte() <= names.end_byte() {
        return false;
    }
    match not_in_force {
        None => true,
        Some(field) => !binder
            .child_by_field_name(field)
            .is_some_and(|part| covers(&part, node)),
    }
}

/// Whether `node` lies inside `outer`'s byte range.
fn covers(outer: &Node<'_>, node: Node<'_>) -> bool {
    node.start_byte() >= outer.start_byte() && node.end_byte() <= outer.end_byte()
}

/// Whether the names under `names` include `name`.
///
/// **Identifiers only, and every identifier in the subtree** — so a destructuring pattern
/// contributes each name it introduces rather than only its head. A child that the spec
/// declares as a symbol contributes nothing, because a name *it* introduces has an entity and
/// this classifier is about the names that have none: a closure's `|x: u32|` is a
/// `parameter` node and is indexed as one, while `|x|` is a bare `identifier` and is not.
/// That distinction is per language and therefore read from the spec rather than written out.
fn binds(spec: &LanguageSpec, names: &Node<'_>, source: &[u8], name: &str) -> bool {
    if spec.symbol_rule(names.kind()).is_some() {
        return false;
    }
    let holds = source
        .get(names.byte_range())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .is_some_and(|text| text == name);
    if holds {
        return true;
    }
    let mut cursor = names.walk();
    names
        .named_children(&mut cursor)
        .any(|child| binds(spec, &child, source, name))
}

#[cfg(test)]
mod tests {
    use super::local_binding;
    use crate::extract::registry;
    use crate::model::{Evidence, Language, RelationKind, ResolutionState};

    /// The evidence class of every reference the extractor emits for `source`, keyed by name.
    ///
    /// Read through the **public** extractor rather than through [`local_binding`] directly, so
    /// what these tests check is what the graph will carry — a classifier that is right and is
    /// never called is not a classifier.
    fn classes(source: &str) -> Vec<(String, &'static str)> {
        let file = crate::extract::walker::extract_with(
            registry::get(Language::Rust).expect("rust spec"),
            crate::model::RepoPath::new("src/lib.rs").expect("valid path"),
            source,
        );
        file.relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::References)
            .filter_map(|relation| {
                relation
                    .resolution
                    .evidence_class()
                    .map(|class| (relation.target_name.clone(), class))
            })
            .collect()
    }

    /// Whether any reference to `name` carries `local_binding`.
    fn is_local(name: &str, source: &str) -> bool {
        classes(source)
            .into_iter()
            .any(|(candidate, class)| candidate == name && class == "local_binding")
    }

    /// Every occurrence the extractor marks local, as its start byte and its name.
    ///
    /// **The byte, and not the name.** `is_local` answers "is this name ever local", which
    /// is a question about the *source* rather than about an occurrence, and it is blind to
    /// the case the two thirds of this module are about: one name that is local in one place
    /// and reaches a declaration in another. `let size = size + 1` has three occurrences of
    /// `size` and they are three different answers, so a test about it has to say which
    /// occurrence it means. Reading the byte is also what makes the assertions checkable by
    /// eye against the source above them.
    fn locals(source: &str) -> Vec<(usize, String)> {
        let file = crate::extract::walker::extract_with(
            registry::get(Language::Rust).expect("rust spec"),
            crate::model::RepoPath::new("src/lib.rs").expect("valid path"),
            source,
        );
        file.relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::References)
            .filter(|relation| relation.resolution.evidence_class() == Some("local_binding"))
            .map(|relation| {
                (
                    relation.span.start_byte as usize,
                    relation.target_name.clone(),
                )
            })
            .collect()
    }

    /// The start byte of the `index`th occurrence of `needle` in `source`.
    ///
    /// Counting occurrences rather than writing offsets out is what keeps a test readable
    /// after the source is reworded, and it is checked rather than trusted: an index past the
    /// end of the list panics here, with the source in the message, instead of silently
    /// comparing against nothing.
    fn occurrence(source: &str, needle: &str, index: usize) -> usize {
        let bytes = source.as_bytes();
        let mut found = Vec::new();
        let mut at = 0;
        while let Some(offset) = source[at..].find(needle) {
            let start = at + offset;
            let end = start + needle.len();
            let whole_word = (start == 0 || !is_word_byte(bytes[start - 1]))
                && (end == bytes.len() || !is_word_byte(bytes[end]));
            if whole_word {
                found.push(start);
            }
            at = end;
        }
        *found.get(index).unwrap_or_else(|| {
            panic!("{source:?} has {index} occurrences of `{needle}`, not one more")
        })
    }

    fn is_word_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }

    /// The binder node type the class records for `name`.
    fn binder_of(name: &str, source: &str) -> Vec<String> {
        let file = crate::extract::walker::extract_with(
            registry::get(Language::Rust).expect("rust spec"),
            crate::model::RepoPath::new("src/lib.rs").expect("valid path"),
            source,
        );
        file.relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::References)
            .filter(|relation| relation.target_name == name)
            .filter_map(|relation| match &relation.resolution {
                ResolutionState::Pending {
                    evidence: Evidence::LocalBinding { binder },
                    ..
                } => Some(binder.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_class_records_which_binder_claims_the_name() {
        // The payload is what lets `peek explain` answer "why is this a local" without
        // re-parsing the file, and it is the one thing in the class that is checkable from the
        // index rather than re-derived.
        let source = "fn f(limit: u8) { for attempt in 0..limit { let _ = attempt; } }";
        assert_eq!(binder_of("attempt", source), vec!["for_expression"; 2]);
        let shadowed = "fn f() { let mut out = String::new(); }";
        assert_eq!(binder_of("out", shadowed), vec!["let_declaration"]);
    }

    #[test]
    fn a_let_binding_makes_every_later_use_of_its_name_local() {
        let source = "fn f() { let mut out = String::new(); out.push_str(\"x\"); }";
        assert!(is_local("out", source), "the use after the pattern");
    }

    #[test]
    fn the_occurrence_that_writes_the_binding_is_local_too() {
        // Ten of the fixture's `binds_nothing` rows are this shape, one per `let` or `for` the
        // fixture declares. A classifier that only looked inside a binder's *scope* would leave
        // every one of them decided and wrong, which is the clause that has to be stated
        // separately rather than folded into the other.
        let source = "fn f() { let mut out = String::new(); }";
        assert!(is_local("out", source), "the declaration itself");
    }

    #[test]
    fn a_for_binding_covers_its_body_through_the_statement_that_wraps_it() {
        // `for …` written as a statement is an `expression_statement` around a
        // `for_expression`, so a scan of a block's direct children finds nothing at all unless
        // one wrapper level is unwrapped.
        let source = "fn f(limit: u8) { for attempt in 0..limit { let _ = attempt; } }";
        assert!(is_local("attempt", source));
    }

    #[test]
    fn a_for_binding_does_not_cover_its_iterable() {
        // The iterable is evaluated before the loop variable exists, so `for x in x` reads an
        // `x` from outside the loop. Reading the grammar's `value` field is what tells the two
        // apart; `attempt` in the body is covered while `limit` never is.
        let source = "fn f(limit: u8) { for attempt in 0..limit { let _ = attempt; } }";
        assert!(
            !is_local("limit", source),
            "the iterable is evaluated before the loop variable is bound"
        );
    }

    #[test]
    fn an_initialiser_is_not_bound_by_the_declaration_it_is_the_initialiser_of() {
        // `let size = size + 1` reads the `size` that existed before the statement, so the
        // occurrence inside the initialiser is **not** local even though the extractor now
        // classifies the declaration itself as local.
        //
        // **Two sources that differ in one thing only, because one source cannot carry this
        // claim.** `size` appears three times in `let size = size + 1` and the three are three
        // different answers, so the question is not "is `size` local" — it is *which*
        // occurrence. Written the obvious way the test contradicts the module: asking whether
        // any `size` is local must be **yes** because of the declaration, and asking for a
        // count of one must be **two** because of the declaration and the read after it. Both
        // of those were asserted here, and both were wrong.
        //
        // Moving the read from inside the initialiser to after it changes nothing else about
        // either source, so the difference between the two answers is exactly the clause.
        let in_initialiser = "fn f(size: u32) { let size = size + 1; }";
        let after = "fn f(size: u32) { let size = 0; let _ = size; }";
        assert_eq!(
            locals(in_initialiser),
            vec![(occurrence(in_initialiser, "size", 1), "size".to_owned())],
            "only the declaration is local: the parameter is an entity and the read inside the \
             initialiser is the `size` that existed before the statement. {in_initialiser:?}"
        );
        assert_eq!(
            locals(after),
            vec![
                (occurrence(after, "size", 1), "size".to_owned()),
                (occurrence(after, "size", 2), "size".to_owned()),
            ],
            "the declaration and the read after it are both local, and the parameter is not. \
             {after:?}"
        );
    }

    #[test]
    fn a_field_name_is_not_classified_by_the_binder_of_its_receiver() {
        // The clause without which the rule is inadmissible: `sink` is a `let`, `text` is a
        // member of the `Sink` it holds, and `Sink.text` resolves correctly today. Classifying
        // it here would replace a correct edge with a gap — which no published column shows
        // and a damage count over labelled rows would call a repair.
        let source = "struct Sink { text: String } fn f() { let sink = Sink { text: String::new() }; sink.text.push_str(\"x\"); }";
        assert!(is_local("sink", source), "the receiver is a local");
        assert!(
            !is_local("text", source),
            "the field is a member of the sink"
        );
    }

    #[test]
    fn a_field_shorthand_is_a_use_of_a_local_and_is_classified_like_one() {
        // `Entry { count }` puts a bare `identifier` in a `shorthand_field_initializer`, which
        // has no field name at all — so there is nothing for clause 1 to match on. With no
        // binder in scope the occurrence keeps `name_only` and the resolver can place it, which
        // is what the fixture needs. Named here because it is the shape a field name and a
        // value read share in one occurrence.
        let source = "struct Entry { count: u32 } fn f(count: u32) -> Entry { Entry { count } }";
        assert!(!is_local("count", source));
    }

    #[test]
    fn a_parameter_use_is_not_local_because_the_index_holds_that_parameter() {
        // The counterweight to every clause above: the rule is about names the index has **no**
        // entity for, and a parameter is an entity.
        let source = "fn f(entries: u32) { let _ = entries; }";
        assert!(!is_local("entries", source));
    }

    #[test]
    fn a_typed_closure_parameter_is_not_local_because_it_is_indexed() {
        // `|x: u32|` is a `parameter` node and the extractor emits a `Parameter` entity for it;
        // `|x|` is a bare `identifier` and it emits none. A classifier that treated the whole
        // closure as entityless would unresolve every use of a typed closure parameter — the
        // same damage the field clause exists to prevent, reached by another route.
        let typed = "fn f() { let g = |x: u32| x + 1; let _ = g(1); }";
        assert!(
            !is_local("x", typed),
            "a typed closure parameter is an entity"
        );
        let untyped = "fn f() { let g = |x| x + 1; let _ = g(1); }";
        assert!(
            is_local("x", untyped),
            "an untyped closure parameter is not"
        );
    }

    #[test]
    fn an_import_alias_is_not_local_because_the_resolver_places_imports() {
        // `use ... as Renamed` binds a local name with no entity, and it is deliberately
        // absent from the binding table: R1 places it from the import binding, and a rule that
        // called it local would destroy a correct import edge.
        let source = "use crate::model::Alias as Renamed; fn f() -> Renamed { todo!() }";
        assert!(!is_local("Renamed", source));
    }

    #[test]
    fn a_match_arm_pattern_is_unclassified_rather_than_local() {
        // Stated as a test so the limit is a check and not only a claim. The table does not
        // name `match_arm`, so `count` here keeps `name_only`: unclassified, which is not the
        // same answer as "not a local", and the difference is why a missing binder is a gap
        // rather than a wrong edge.
        let source = "struct Entry { count: u32 } fn f(e: Entry) { match e { Entry { count } => { let _ = count; } } }";
        assert!(!is_local("count", source));
    }

    #[test]
    fn the_classifier_returns_nothing_for_a_language_that_declares_no_binders() {
        // A spec with an empty table classifies nothing, which is the honest answer and not the
        // same as one that classifies everything. Guarding it stops a table from silently
        // becoming a keyword match in the shared walker.
        let mut spec = *registry::get(Language::Rust).expect("rust spec");
        spec.bindings = &[];
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("the rust grammar loads");
        let tree = parser
            .parse("fn f() { let out = 1; let _ = out; }", None)
            .expect("the source parses");
        let found = tree
            .root_node()
            .descendant_for_byte_range(13, 16)
            .map(|node| local_binding(&spec, node, b"fn f() { let out = 1; let _ = out; }"));
        assert_eq!(
            found.flatten(),
            None,
            "an empty binding table classifies nothing"
        );
    }
}
