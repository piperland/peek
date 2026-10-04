//! The reference-extraction claim, checked against the engine rather than inferred.
//!
//! The gate measures `references` at 0 of 18. That number is only a finding about
//! the *engine* if the zero comes from the engine and not from the harness, so this
//! file separates the two by asking the engine directly, on a string, with no
//! fixture and no gate.
//!
//! What it establishes:
//!
//! 1. `extract_with` on a file with an obvious identifier use produces **zero**
//!    `References` relations.
//! 2. The reason is visible in `LanguageSpec::references.excluded_parents`: it names
//!    every node type the spec declares as a symbol, and the walker's
//!    `is_excluded_reference` walks *up* to the nearest declared ancestor. An
//!    identifier inside a function therefore finds `function_item` and is excluded,
//!    which is the opposite of what the field's own comment says it is for.
//! 3. An identifier at file level — inside a `use`, say — is *not* excluded, but is
//!    still dropped, because reference emission requires an enclosing entity and
//!    there is none at file level.
//!
//! The two together mean the `References` class is unreachable in this build, not
//! merely rare. That is the difference between "0.00 recall" and "there is nothing
//! here to be right about", and the distinction is worth a test of its own because
//! the first reading of the matrix column would be the wrong one.

#![allow(clippy::expect_used, clippy::panic)]

use peek_core::extract::{LanguageSpec, extract_with};
use peek_core::model::{Language, RelationKind, RepoPath};

/// One file, small enough to read, with three kinds of identifier use.
const SOURCE: &str = "\
pub fn outer() -> u32 {
    let total = inner();
    total + 1
}

pub fn inner() -> u32 {
    2
}
";

#[test]
fn the_spec_declares_a_reference_rule() {
    // If this fails the other tests in this file are measuring a gate artefact, so
    // it is checked first and by name.
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
fn every_excluded_parent_is_a_node_type_the_spec_itself_declares() {
    // The mechanism behind the zero. `excluded_parents` is documented as the list
    // of node types whose *own name* must not also be emitted as a reference to
    // itself. Listing every declared symbol node type achieves something stronger:
    // nothing inside any declaration is ever a reference, because the walker walks
    // up to the nearest declared ancestor and asks whether that ancestor is listed.
    let spec = LanguageSpec::for_language(Language::Rust).expect("rust has a spec");
    let references = spec
        .references
        .as_ref()
        .expect("the rust spec declares a reference rule");
    let declared: Vec<&str> = spec.symbols.iter().map(|rule| rule.node_type).collect();
    let excluded = references.excluded_parents;
    assert_eq!(
        excluded.len(),
        declared.len(),
        "the excluded list and the declared symbol list have drifted apart"
    );
    for node_type in excluded {
        assert!(
            declared.contains(&node_type),
            "`{node_type}` is excluded from reference extraction but is not a declared symbol, \
             so nothing declares what it was excluded from"
        );
    }
}

#[test]
fn extracting_a_file_with_identifier_uses_produces_no_reference_relations() {
    // The measurement itself, with no fixture and no gate in the way.
    let spec = LanguageSpec::for_language(Language::Rust).expect("rust has a spec");
    let path = RepoPath::new("probe.rs").expect("a repository path");
    let extracted = extract_with(spec, path, SOURCE);

    let references: Vec<&str> = extracted
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::References)
        .map(|relation| relation.target_name.as_str())
        .collect();
    assert!(
        references.is_empty(),
        "this file uses `inner` and `total`; references were emitted for {references:?}"
    );
}

#[test]
fn the_same_file_does_produce_call_relations_so_extraction_is_working() {
    // Without this, the test above would pass for the wrong reason — a spec that
    // matches nothing at all also produces no references.
    let spec = LanguageSpec::for_language(Language::Rust).expect("rust has a spec");
    let path = RepoPath::new("probe.rs").expect("a repository path");
    let extracted = extract_with(spec, path, SOURCE);

    let calls: Vec<&str> = extracted
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Calls)
        .map(|relation| relation.target_name.as_str())
        .collect();
    assert_eq!(
        calls,
        vec!["inner"],
        "`let total = inner();` is the one call in this file; extraction reached it, so the \
         absence of references is specific to the reference rule"
    );

    let symbols: Vec<&str> = extracted
        .entities
        .iter()
        .map(|entity| entity.id().qualified_name())
        .collect();
    assert!(
        symbols.contains(&"outer") && symbols.contains(&"inner"),
        "both declarations were extracted; the file is parsed and walked: {symbols:?}"
    );
}

#[test]
fn a_file_level_identifier_is_not_excluded_but_is_still_dropped() {
    // The second half of the mechanism, and the reason the fix is not a one-line
    // change to `excluded_parents`. Even with the exclusion list emptied, an
    // identifier with no enclosing entity never reaches `emit_reference`.
    let spec = LanguageSpec::for_language(Language::Rust).expect("rust has a spec");
    let path = RepoPath::new("probe.rs").expect("a repository path");
    let with_a_use = "\
use other::Thing;

pub fn make() -> Thing {
    Thing::new()
}
";
    let extracted = extract_with(spec, path, with_a_use);
    let references: Vec<&str> = extracted
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::References)
        .map(|relation| relation.target_name.as_str())
        .collect();
    assert!(
        references.is_empty(),
        "`use other::Thing;` names identifiers at file level with no enclosing entity; \
         references were emitted for {references:?}"
    );

    // And the `use` itself did produce an import, so the file was walked.
    assert!(
        extracted
            .relations
            .iter()
            .any(|relation| relation.kind == RelationKind::Imports),
        "the `use` produced an import relation, so the file was walked and the missing \
         references are not a parse failure"
    );
}