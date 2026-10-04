//! The reference-extraction rule, checked against the engine rather than inferred.
//!
//! The gate measured `references` at 0 of 18, and the first question was whether that zero came
//! from the engine or from the harness. It came from the engine, and it came from one rule: the
//! list of node types excluded from reference extraction named **every node type the Rust spec
//! declares as a symbol**, and the walker asked that list about the *nearest enclosing
//! declaration* rather than about the occurrence. An identifier inside a function therefore found
//! `function_item` in the list and was dropped, and so was every other identifier in every other
//! body — the class was unreachable rather than rare.
//!
//! What this file establishes, on a string, with no fixture and no gate:
//!
//! 1. **A use of a name in a body is a reference.** `References` is produced, by the same walk
//!    that produces the declaration's entity.
//! 2. **A declaration's own name is not a reference to itself.** The exclusion is real; it was
//!    only being asked the wrong question.
//! 3. **The exclusion is asked about the occurrence, not the subtree.** A shadowing parameter
//!    separates the two, and it is the discriminating test.
//! 4. **A file-level identifier is a different question**, answered separately at the bottom.
//!
//! Point 3 is the test that matters. A rule that dropped everything beneath a declared node type
//! would fail it; a rule that stopped at the immediate parent would fail it too, because the uses
//! in a body are several levels below the declaration that encloses them. Only asking "is this
//! occurrence the name that declaration introduced" gets all three right, which is why the walker
//! compares **nodes** rather than names.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use peek_core::extract::{LanguageSpec, extract_with};
use peek_core::model::{EntityKind, Language, RelationKind, RepoPath};

/// One file, small enough to read, with a call, a local, and two declarations.
const SOURCE: &str = "\
pub fn outer() -> u32 {
    let total = inner();
    total + 1
}

pub fn inner() -> u32 {
    2
}
";

/// How many times `source` was recorded as referencing each name, in this file.
///
/// Counts rather than a list, because the order two levels of a nested call expression are
/// visited in is a property of the grammar rather than a claim about what a reference is, and a
/// test that pinned it would fail on a grammar upgrade for no reason a reader could check.
fn reference_counts(source: &str, text: &str) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for name in reference_names(source, text) {
        *counts.entry(name).or_default() += 1;
    }
    counts
}

/// Every `references` target name this file produced, from `source`.
fn reference_names(source: &str, text: &str) -> Vec<String> {
    let spec = LanguageSpec::for_language(Language::Rust).expect("rust has a spec");
    let path = RepoPath::new("probe.rs").expect("a repository path");
    let extracted = extract_with(spec, path, text);
    extracted
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::References)
        .filter(|relation| relation.source.qualified_name() == source)
        .map(|relation| relation.target_name.clone())
        .collect()
}

#[test]
fn the_spec_declares_a_reference_rule() {
    // If this fails the other tests in this file are measuring a gate artefact, so it is checked
    // first and by name.
    let spec = LanguageSpec::for_language(Language::Rust).expect("rust has a spec");
    let references = spec
        .references
        .as_ref()
        .expect("the rust spec declares a reference rule");
    assert!(
        !references.node_types.is_empty(),
        "the rule names no node types, so nothing can match it"
    );
}

#[test]
fn a_use_of_a_name_in_a_body_is_a_reference() {
    // The measurement itself, with no fixture and no gate in the way. `inner` is called and
    // `total` is read; both are uses of a name by the body that holds them.
    let counts = reference_counts("outer", SOURCE);
    assert_eq!(
        counts.get("inner").copied(),
        Some(1),
        "`let total = inner();` names `inner` once; the reference counts are {counts:?}"
    );
    assert_eq!(
        counts.get("total").copied(),
        Some(2),
        "`let total = ...` binds `total` and `total + 1` reads it, so the name occurs twice and \
         both are uses; the reference counts are {counts:?}"
    );
}

#[test]
fn a_declarations_own_name_is_not_a_reference_to_itself() {
    // The other half of the same walk, and the reason the rule exists at all. `outer` declares
    // the name `outer`; that occurrence introduces the name rather than using it.
    let counts = reference_counts("outer", SOURCE);
    assert!(
        !counts.contains_key("outer"),
        "`pub fn outer` declares the name `outer` and never uses it; declaring a name is not a \
         use of it, and the reference counts are {counts:?}"
    );
}

#[test]
fn the_exclusion_is_asked_about_the_occurrence_and_not_the_subtree() {
    // The discriminating case. `size` occurs three times in this body: the parameter's own
    // binding, the value the rebinding reads, and the value the function returns. The first is a
    // declaration and the other two are uses, and **only the declaration is inside the
    // `parameter`**. The uses are several syntactic levels below the `parameter` that binds the
    // name and their immediate parents are a `let_declaration` and a `block`.
    //
    // So a rule that dropped every identifier beneath a declared node type would drop all three,
    // and a rule that stopped at the immediate parent would drop the two uses. Both answers are
    // wrong here, which is what makes this the test rather than another restatement of the pair
    // above.
    const SHADOWED: &str = "\
pub fn outer(size: usize) -> usize {
    let size = size + 1;
    size
}
";
    assert_eq!(
        reference_names("outer.size", SHADOWED),
        Vec::<String>::new(),
        "the parameter declares the name `size`; the only identifier inside it is that \
         declaration, and a declaration is not a use of the name it introduces"
    );

    let counts = reference_counts("outer", SHADOWED);
    assert_eq!(
        counts.get("size").copied(),
        Some(3),
        "the rebinding's left-hand pattern, the value it reads, and the returned value are three \
         uses of the name `size` in the body of `outer`; the reference counts are {counts:?}"
    );
}

#[test]
fn a_use_is_found_at_the_depth_it_is_written() {
    // The other direction the exclusion has to survive: a declaration's name is one level below
    // the declaration, and the uses in its body are up to six. Both are decided in one walk, so
    // no rule with a single depth can produce this pair.
    const NESTED: &str = "\
pub fn outer() -> u32 {
    let values = vec![inner(), inner()];
    values.first().copied().unwrap_or_default()
}

pub fn inner() -> u32 {
    2
}
";
    let counts = reference_counts("outer", NESTED);
    assert_eq!(
        counts.get("inner").copied(),
        Some(2),
        "`vec![inner(), inner()]` names `inner` twice, inside a macro's token tree and inside an \
         array; the reference counts are {counts:?}"
    );
    assert_eq!(
        counts.get("values").copied(),
        Some(3),
        "the local is named by the `let`, read as the receiver of `first`, and read again as the \
         receiver of `copied`; the reference counts are {counts:?}"
    );
    assert_eq!(
        counts.get("vec").copied(),
        Some(1),
        "`vec!` names the macro; the reference counts are {counts:?}"
    );
    assert_eq!(
        counts.get("first").copied(),
        Some(2),
        "`values.first()` appears twice, once as the receiver of `copied` and once as the \
         statement; the reference counts are {counts:?}"
    );
    assert!(
        !counts.contains_key("outer"),
        "`outer` is declared and never used inside itself; the reference counts are {counts:?}"
    );
}

#[test]
fn a_field_declarations_type_is_a_use_and_its_own_name_is_not() {
    // The same distinction one level down, in a declaration that has no body. `Holder.width`
    // declares the name `width` and names the type `Width`; the first is a declaration and the
    // second is a use, in the same subtree of the same node type.
    const HOLDER: &str = "\
pub struct Width;

pub struct Holder {
    pub width: Width,
}
";
    assert_eq!(
        reference_names("Holder.width", HOLDER),
        vec!["Width".to_owned()],
        "the field declares `width` and names the type `Width`, which is a use of a name the \
         file declares elsewhere"
    );
}

#[test]
fn a_file_level_identifier_is_a_separate_question_and_is_still_dropped() {
    // Emission requires an **enclosing declaration**, not merely an enclosing node, and that is a
    // different rule from the one above: it is about where a relation originates, not about which
    // occurrence is a use. A `use` binds names, the import relation already records the binding,
    // and a reference anchored on the file entity would be a second record of the same fact.
    //
    // Pinned separately because it is the other half of the original zero. Emptying the exclusion
    // list alone would not have produced a single reference in this file, so a fix that measured
    // only the list could have reported the class as reachable while it stayed empty.
    let spec = LanguageSpec::for_language(Language::Rust).expect("rust has a spec");
    let path = RepoPath::new("probe.rs").expect("a repository path");
    let extracted = extract_with(
        spec,
        path,
        "\
use other::Thing;

pub fn make() -> Thing {
    Thing::new()
}
",
    );
    assert!(
        extracted
            .relations
            .iter()
            .any(|relation| relation.kind == RelationKind::Imports),
        "the `use` produced an import relation, so the file was walked and the absence of a \
         file-level reference is a placement rule rather than a parse failure"
    );
    let from_the_file: Vec<&str> = extracted
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::References)
        .filter(|relation| relation.source.kind() == EntityKind::File)
        .map(|relation| relation.target_name.as_str())
        .collect();
    assert!(
        from_the_file.is_empty(),
        "`use other::Thing;` binds a name the import relation already records; a reference \
         anchored on the file was emitted for {from_the_file:?}"
    );
}
