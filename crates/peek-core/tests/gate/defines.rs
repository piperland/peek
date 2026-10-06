//! Who defines each declaration, counted over the stored graph.
//!
//! D-0037's population is incoming `Defines` edges: `Defines(X, Y)` holds when Y's declaration
//! is written where X is, and every entity carries exactly one. A `Contains` edge says where a
//! declaration sits, not who declared it, so it cannot stand in for one — an impl block
//! contains its methods without defining them, and a function contains its parameters without
//! defining them. This module counts the population the `definitions` dimension scores; the
//! extractor (not a query-time derivation) is what has to produce it, because a derived edge is
//! not in the stored graph this reads.

use std::collections::{BTreeMap, BTreeSet};

use super::measure::Graph;

/// Distinct definers per rendered identity: the sources of the stored `Defines` edges pointing
/// at it. Two `impl` blocks for one type are two entities with two edges from one file, so what
/// is counted is definers rather than edges — one declaration site, one claim.
pub fn definers(graph: &Graph) -> BTreeMap<String, BTreeSet<String>> {
    let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for relation in &graph.relations {
        if relation.kind != "defines" {
            continue;
        }
        if let Some(target) = &relation.target {
            found
                .entry(target.key().render())
                .or_default()
                .insert(relation.source.key().render());
        }
    }
    found
}
