//! Row encoding and decoding.
//!
//! # The one rule that matters
//!
//! Every conversion between a stored row and a model type is a *checked* conversion, and a
//! failure is an error rather than a default. Audit B1 is the case: Cortex's
//! `GraphNode.path: Option<PathBuf>` failed serde on a non-UTF-8 path, and because that error
//! was discarded, one unreadable path poisoned the entire store write — zero bytes on disk and
//! one "indexed N files" message. Decoding here never panics, never substitutes a default, and
//! never silently drops a field.
//!
//! Enumerations are stored as their serde wire form (`"method"`, `"qualified_name_in_scope"`) so
//! that a row stays readable and greppable, and so there is exactly one definition of each
//! variant's name rather than a hand-written table that can drift from the model.

use rusqlite::Row;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::model::entity::{Entity, EntityId, EntityKind};
use crate::model::language::Language;
use crate::model::path::RepoPath;
use crate::model::relation::{Relation, RelationKind, ResolutionState, UnresolvedReason};
use crate::model::span::Span;

use super::error::StoreError;

/// Entity columns, in the order [`entity_from_row`] expects them.
pub const ENTITY_COLUMNS: &str = "path, kind, qualified_name, entity_ordinal, name, signature, \
doc, start_byte, end_byte, start_line, start_column, end_line, end_column, language, is_test, \
structural_fingerprint";

/// Relation columns, in the order [`relation_from_row`] expects them.
pub const RELATION_COLUMNS: &str = "id, kind, source_path, source_kind, source_qualified_name, \
source_ordinal, target_name, target_path, target_kind, target_qualified_name, target_ordinal, \
start_byte, end_byte, start_line, start_column, end_line, end_column, resolution_state, \
resolution_json";

/// A relation row plus its surrogate key, so candidates can be attached afterwards.
pub struct RelationRow {
    /// The `relation.id` surrogate that `relation_candidate` points at.
    pub id: i64,
    /// The decoded relation. An ambiguous relation arrives here with an empty candidate list;
    /// the caller attaches the real one from `relation_candidate` in a second query.
    pub relation: Relation,
}

/// Serialise any model enum to its stored wire form.
///
/// Goes through serde rather than a hand-written `as_str` lookup so the stored text and the
/// serialised text cannot diverge: there is one definition of each variant's name.
/// The wire form of a language, kind, or relation kind.
///
/// These go through the type's own `as_str()` rather than `serde`, because serde's
/// `rename_all = "snake_case"` produces a *different* spelling for some variants than the
/// canonical name does — `Language::ObjectiveC` serialises as `objective_c` while `as_str()`
/// says `objectivec`. Two spellings of one value is two sources of truth, and a migration that
/// reads one and writes the other silently changes the stored value. The canonical
/// `as_str()`/`FromStr` pair is the contract, and `model::Language` already round-trips it.
pub fn enum_to_sql<T: Serialize>(value: &T) -> Result<String, StoreError> {
    let json = serde_json::to_value(value)
        .map_err(|e| StoreError::Query(format!("cannot encode a stored value: {e}")))?;
    json.as_str()
        .map(str::to_owned)
        .ok_or_else(|| StoreError::Query("an enum did not encode to a string".to_owned()))
}

/// Decode a stored enum value, rejecting anything this build does not know.
///
/// A newer Peek writing a kind or evidence class this build has never heard of must produce an
/// error. Defaulting to a "closest known" variant would be a silent misread — the failure mode
/// the schema version check exists to rule out, reached by a different route.
pub fn enum_from_sql<T: DeserializeOwned>(text: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
        .map_err(|e| StoreError::Query(format!("cannot decode stored value {text:?}: {e}")))
}

/// [`enum_to_sql`] specialised to [`EntityKind`], for use in a `params!` list.
pub fn kind_to_sql(kind: EntityKind) -> Result<String, StoreError> {
    Ok(kind.as_str().to_owned())
}

/// [`enum_to_sql`] specialised to [`RelationKind`], for use in a `params!` list.
pub fn relation_kind_to_sql(kind: RelationKind) -> Result<String, StoreError> {
    Ok(kind.as_str().to_owned())
}

/// [`enum_to_sql`] specialised to [`Language`], for use in a `params!` list.
pub fn language_to_sql(language: Language) -> Result<String, StoreError> {
    Ok(language.as_str().to_owned())
}

/// The discriminator stored in `resolution_state`.
///
/// A separate column from the payload so the table can be filtered by state with an index seek
/// instead of parsing JSON for every row. Audit B11 is the failure this prevents: state that
/// exists only inside a blob cannot be counted, so it cannot be reported honestly.
pub fn resolution_tag(state: &ResolutionState) -> &'static str {
    match state {
        ResolutionState::Pending { .. } => "pending",
        ResolutionState::Resolved { .. } => "resolved",
        ResolutionState::Ambiguous { .. } => "ambiguous",
        ResolutionState::Unresolved { .. } => "unresolved",
        ResolutionState::Inferred { .. } => "inferred",
    }
}

/// The state as a JSON payload for `resolution_json`.
///
/// An ambiguous relation's candidate list is deliberately *not* included: it is stored
/// relationally in `relation_candidate` so that it is queryable, and duplicating it here would
/// create two sources of truth that could disagree (contract H5). The payload records the state
/// and its evidence; the list is re-attached on read.
pub fn resolution_payload(state: &ResolutionState) -> Result<String, StoreError> {
    let payload_state = match state {
        // The stand-in the reader recognises: `resolution_from_sql` treats exactly this
        // combination as "ambiguous, list to be loaded from the table".
        ResolutionState::Ambiguous { .. } => ResolutionState::Unresolved {
            reason: UnresolvedReason::Ambiguous,
        },
        other => other.clone(),
    };
    serde_json::to_string(&payload_state)
        .map_err(|e| StoreError::Query(format!("cannot encode a resolution state: {e}")))
}

/// Whether a decoded state is ambiguous, i.e. whether its candidate list still needs loading.
pub fn is_ambiguous(state: &ResolutionState) -> bool {
    matches!(state, ResolutionState::Ambiguous { .. })
}

/// Read a text column, mapping NULL to `None`.
fn optional_text(row: &Row<'_>, index: usize) -> Result<Option<String>, StoreError> {
    row.get::<_, Option<String>>(index)
        .map_err(|e| StoreError::Query(format!("column {index} is not readable text: {e}")))
}

/// Read a required text column, treating a NULL as corruption rather than as an empty string.
fn text(row: &Row<'_>, index: usize) -> Result<String, StoreError> {
    optional_text(row, index)?.ok_or_else(|| {
        StoreError::Corrupt(format!(
            "column {index} is NULL but the schema requires a value"
        ))
    })
}

/// Read a required integer column, treating a NULL as corruption.
fn int(row: &Row<'_>, index: usize) -> Result<i64, StoreError> {
    row.get::<_, Option<i64>>(index)
        .map_err(|e| StoreError::Query(format!("column {index} is not readable integer: {e}")))?
        .ok_or_else(|| {
            StoreError::Corrupt(format!(
                "column {index} is NULL but the schema requires a value"
            ))
        })
}

/// Narrow a stored `i64` to the `u32` the model uses, rejecting overflow.
fn to_u32(value: i64, what: &str) -> Result<u32, StoreError> {
    u32::try_from(value)
        .map_err(|e| StoreError::Corrupt(format!("{what} value {value} does not fit in u32: {e}")))
}

/// Rebuild a [`RepoPath`], rejecting a stored value that no longer validates.
///
/// A path written by a build with different normalisation rules must not be silently accepted:
/// `RepoPath` guarantees a path cannot escape the repository root, and honouring that guarantee
/// requires re-validating what comes back *out* of the database, not only what goes in.
pub fn path_from_sql(stored: &str) -> Result<RepoPath, StoreError> {
    RepoPath::new(stored).ok_or_else(|| {
        StoreError::Corrupt(format!(
            "stored path {stored:?} is not a valid repository path"
        ))
    })
}

/// Rebuild an [`EntityId`] from its four decomposed columns.
fn entity_id_from_row(
    row: &Row<'_>,
    path_column: usize,
    kind_column: usize,
    qname_column: usize,
    ordinal_column: usize,
) -> Result<EntityId, StoreError> {
    let path = path_from_sql(&text(row, path_column)?)?;
    let kind: EntityKind = enum_from_sql(&text(row, kind_column)?)?;
    let qualified_name = text(row, qname_column)?;
    let ordinal = to_u32(int(row, ordinal_column)?, "entity ordinal")?;
    Ok(EntityId::new(path, kind, qualified_name, ordinal))
}

/// Rebuild the optional span occupying six columns starting at `start`.
///
/// The schema's CHECK guarantees all-or-nothing, so a partial span cannot be written through the
/// public API. It is still checked rather than assumed: reading a half-stored span as a
/// zero-width range would report a *wrong location* rather than an error, which is the worse
/// outcome for a code-intelligence tool.
fn span_from_columns(row: &Row<'_>, start: usize) -> Result<Option<Span>, StoreError> {
    let mut values = [0_i64; 6];
    let mut present = 0;
    for (offset, slot) in values.iter_mut().enumerate() {
        let value = row
            .get::<_, Option<i64>>(start + offset)
            .map_err(|e| StoreError::Query(format!("span column {offset} is unreadable: {e}")))?;
        if let Some(value) = value {
            *slot = value;
            present += 1;
        }
    }
    if present == 0 {
        return Ok(None);
    }
    if present != values.len() {
        return Err(StoreError::Corrupt(format!(
            "span is only partly stored ({present} of 6 columns)"
        )));
    }
    Span::new(
        to_u32(values[0], "start_byte")?,
        to_u32(values[1], "end_byte")?,
        to_u32(values[2], "start_line")?,
        to_u32(values[3], "start_column")?,
        to_u32(values[4], "end_line")?,
        to_u32(values[5], "end_column")?,
    )
    .map(Some)
    .ok_or_else(|| StoreError::Corrupt("stored span has an inverted byte range".to_owned()))
}

/// Decode a full entity row.
pub fn entity_from_row(row: &Row<'_>) -> Result<Entity, StoreError> {
    let id = entity_id_from_row(row, 0, 1, 2, 3)?;
    let name = text(row, 4)?;
    let signature = optional_text(row, 5)?;
    let doc = optional_text(row, 6)?;
    let span = span_from_columns(row, 7)?;
    let language = match optional_text(row, 13)? {
        Some(stored) => Some(stored.parse::<Language>().map_err(|e| {
            StoreError::Query(format!("cannot decode stored language {stored:?}: {e}"))
        })?),
        None => None,
    };
    let is_test = int(row, 14)? != 0;
    let structural_fingerprint = optional_text(row, 15)?;
    Ok(Entity {
        id,
        name,
        signature,
        doc,
        span,
        language,
        is_test,
        structural_fingerprint,
    })
}

/// Decode a full relation row, minus the candidate list.
pub fn relation_from_row(row: &Row<'_>) -> Result<RelationRow, StoreError> {
    let id = int(row, 0)?;
    let kind: RelationKind = enum_from_sql(&text(row, 1)?)?;
    let source = entity_id_from_row(row, 2, 3, 4, 5)?;
    let target_name = text(row, 6)?;

    // A target is present in all four columns or in none. The schema enforces it; re-checking
    // here means a row written under different rules cannot be half-read.
    //
    // The ordinal is an INTEGER column, so it is read as an integer. Reading it as text first —
    // because it sits in the same block as the three TEXT columns — makes `optional_text` fail
    // with "Invalid column type Integer", which is a confusing way to learn that a column's
    // SQLite storage class is not the type the reader assumed.
    let path_slot = optional_text(row, 7)?;
    let kind_slot = optional_text(row, 8)?;
    let name_slot = optional_text(row, 9)?;
    let ordinal_slot = row
        .get::<_, Option<i64>>(10)
        .map_err(|e| StoreError::Query(format!("target ordinal is not readable: {e}")))?;
    let present = [
        path_slot.is_some(),
        kind_slot.is_some(),
        name_slot.is_some(),
        ordinal_slot.is_some(),
    ]
    .iter()
    .filter(|present| **present)
    .count();

    let target = match present {
        0 => None,
        4 => {
            let stored_kind: EntityKind = enum_from_sql(&slot(kind_slot, id)?)?;
            Some(EntityId::new(
                path_from_sql(&slot(path_slot, id)?)?,
                stored_kind,
                slot(name_slot, id)?,
                to_u32(ordinal_slot.unwrap_or_default(), "target ordinal")?,
            ))
        }
        other => {
            return Err(StoreError::Corrupt(format!(
                "relation {id} has a partly-stored target ({other} of 4 columns)"
            )));
        }
    };

    let span = span_from_columns(row, 11)?
        .ok_or_else(|| StoreError::Corrupt(format!("relation {id} has no span")))?;
    let tag = text(row, 17)?;
    let payload = text(row, 18)?;
    let resolution = resolution_from_sql(&tag, &payload, target.is_some())?;

    Ok(RelationRow {
        id,
        relation: Relation {
            kind,
            source,
            target_name,
            target,
            span,
            resolution,
        },
    })
}

/// Unwrap one of the four target columns, which the all-or-nothing check has already proven
/// present.
fn target_column(columns: &[Option<String>; 4], index: usize) -> Result<&str, StoreError> {
    columns[index].as_deref().ok_or_else(|| {
        StoreError::Corrupt("target column is NULL after an all-or-nothing check".to_owned())
    })
}

/// Rebuild a [`ResolutionState`] from its tag and payload.
///
/// The tag says *which* state the row is and the payload supplies the evidence. A disagreement
/// between the two means the row is internally inconsistent, which is a corruption signal rather
/// than something to paper over: trusting either one alone would let a partially-written row be
/// read as a different kind of claim from the one it makes.
fn resolution_from_sql(
    tag: &str,
    payload: &str,
    has_target: bool,
) -> Result<ResolutionState, StoreError> {
    let stored: ResolutionState = serde_json::from_str(payload).map_err(|e| {
        StoreError::Corrupt(format!("resolution payload {payload:?} is unreadable: {e}"))
    })?;
    // The evidence was decoded by the `from_str` above, so a variant this build does not know
    // has already failed; nothing further needs validating. What still needs checking is that the
    // tag and the payload describe the *same* state.
    let rebuilt = match (tag, &stored) {
        ("resolved", ResolutionState::Resolved { by }) => {
            ResolutionState::Resolved { by: by.clone() }
        }
        ("unresolved", ResolutionState::Unresolved { reason }) => ResolutionState::Unresolved {
            reason: reason.clone(),
        },
        ("inferred", ResolutionState::Inferred { by, basis }) => ResolutionState::Inferred {
            by: by.clone(),
            basis: basis.clone(),
        },
        (
            "ambiguous",
            ResolutionState::Unresolved {
                reason: UnresolvedReason::Ambiguous,
            },
        ) => ResolutionState::Ambiguous {
            // The candidate list is loaded from `relation_candidate` by the caller.
            candidates: Vec::new(),
        },
        _ => {
            return Err(StoreError::Corrupt(format!(
                "resolution_state {tag:?} disagrees with its payload"
            )));
        }
    };

    // A followable edge must name its target. Handing a consumer a `Resolved` relation with no
    // target would be exactly the confidently-wrong edge D-0003 exists to prevent.
    if rebuilt.is_resolved() && !has_target {
        return Err(StoreError::Corrupt(format!(
            "relation claims to be {tag:?} but names no target"
        )));
    }
    Ok(rebuilt)
}

/// Decode a `relation_candidate` row whose first two columns are `relation_id` and `ordinal`.
pub fn candidate_from_row(row: &Row<'_>) -> Result<EntityId, StoreError> {
    entity_id_from_row(row, 2, 3, 4, 5)
}

/// Decode a candidate row that *is* the four identity columns, with no leading bookkeeping.
///
/// The join form in [`Store::ambiguous_candidates`] selects only those columns, so it cannot use
/// [`candidate_from_row`]; passing the base offset keeps one decoder rather than two that can
/// disagree about column order. A candidate is an identity and nothing else — it deliberately has
/// no name, signature, or span, because it may name an entity in a file this store has never
/// indexed.
pub fn candidate_from_bare_row(row: &Row<'_>) -> Result<EntityId, StoreError> {
    entity_id_from_row(row, 0, 1, 2, 3)
}

/// One of the four target columns, guaranteed present by the caller's arity check.
fn slot(value: Option<String>, relation: i64) -> Result<String, StoreError> {
    value.ok_or_else(|| {
        StoreError::Corrupt(format!(
            "relation {relation} has a partly-stored target column"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ENTITY_COLUMNS, RELATION_COLUMNS, enum_from_sql, enum_to_sql, is_ambiguous, kind_to_sql,
        language_to_sql, path_from_sql, relation_kind_to_sql, resolution_payload, resolution_tag,
    };
    use crate::model::entity::EntityKind;
    use crate::model::language::Language;
    use crate::model::relation::{Evidence, RelationKind, ResolutionState, UnresolvedReason};
    use crate::store::error::StoreError;

    #[test]
    fn column_lists_match_the_decoders() {
        assert_eq!(
            ENTITY_COLUMNS.split(", ").count(),
            16,
            "entity_from_row reads 16 columns"
        );
        assert_eq!(
            RELATION_COLUMNS.split(", ").count(),
            19,
            "relation_from_row reads 19 columns"
        );
    }

    #[test]
    fn enum_wire_forms_round_trip() {
        for kind in [EntityKind::Method, EntityKind::Trait, EntityKind::TypeAlias] {
            let text = kind_to_sql(kind).expect("encode");
            let back: EntityKind = enum_from_sql(&text).expect("decode");
            assert_eq!(back, kind, "round trip changed {kind}");
        }
        for kind in [
            RelationKind::Calls,
            RelationKind::Implements,
            RelationKind::UsesType,
        ] {
            let text = relation_kind_to_sql(kind).expect("encode");
            let back: RelationKind = enum_from_sql(&text).expect("decode");
            assert_eq!(back, kind, "round trip changed {kind}");
        }
        for language in [Language::Rust, Language::TypeScript, Language::CSharp] {
            let text = language_to_sql(language).expect("encode");
            let back: Language = text.parse().expect("decode");
            assert_eq!(back, language);
        }
    }

    #[test]
    fn the_stored_spelling_and_the_printed_spelling_never_diverge() {
        // This is the guard for the bug that made `Language::ObjectiveC` store as
        // `objective_c` while `Language::as_str()` printed `objectivec`: two spellings of one
        // value, so a migration that read one and wrote the other silently changed stored data.
        // Serialising through `as_str()` and printing through `as_str()` cannot drift.
        for language in Language::ALL {
            assert_eq!(
                language_to_sql(*language).expect("encode"),
                language.as_str(),
                "{language:?} stores and prints differently"
            );
        }
        for kind in [
            EntityKind::Method,
            EntityKind::Trait,
            EntityKind::TypeAlias,
            EntityKind::TypeParameter,
            EntityKind::Static,
        ] {
            assert_eq!(
                kind_to_sql(kind).expect("encode"),
                kind.as_str(),
                "{kind:?} stores and prints differently"
            );
        }
        for kind in [
            RelationKind::UsesType,
            RelationKind::ConfiguredBy,
            RelationKind::TestedBy,
        ] {
            assert_eq!(
                relation_kind_to_sql(kind).expect("encode"),
                kind.as_str(),
                "{kind:?} stores and prints differently"
            );
        }
    }

    #[test]
    fn kind_wire_forms_are_readable_english() {
        // A row a human can read in `sqlite3` is worth more than a few bytes, and someone
        // debugging a bad index should not have to look up an enum discriminant. Crucially the
        // stored spelling is the *printed* spelling — see the divergence guard below.
        assert_eq!(
            kind_to_sql(EntityKind::TypeAlias).expect("encode"),
            "type_alias"
        );
        assert_eq!(
            relation_kind_to_sql(RelationKind::UsesType).expect("encode"),
            "uses_type"
        );
        assert_eq!(
            language_to_sql(Language::ObjectiveC).expect("encode"),
            "objectivec"
        );
    }

    #[test]
    fn an_unknown_stored_enum_value_is_an_error_not_a_default() {
        // The case a schema version check does not cover on its own: a row naming a value this
        // build has never heard of. Substituting a plausible variant would report a resolution
        // claim the file never made.
        let outcome: Result<EntityKind, StoreError> = enum_from_sql("nonexistent_kind");
        assert!(matches!(outcome, Err(StoreError::Query(_))));
    }

    #[test]
    fn resolution_tags_are_exactly_the_four_stored_values() {
        assert_eq!(
            resolution_tag(&ResolutionState::Resolved {
                by: Evidence::UniqueName
            }),
            "resolved"
        );
        assert_eq!(
            resolution_tag(&ResolutionState::Ambiguous {
                candidates: Vec::new()
            }),
            "ambiguous"
        );
        assert_eq!(
            resolution_tag(&ResolutionState::Unresolved {
                reason: UnresolvedReason::External
            }),
            "unresolved"
        );
        assert_eq!(
            resolution_tag(&ResolutionState::Inferred {
                by: Evidence::NameOnly,
                basis: "heuristic".to_owned()
            }),
            "inferred"
        );
    }

    #[test]
    fn an_ambiguous_payload_records_the_state_but_not_the_candidate_list() {
        let payload = resolution_payload(&ResolutionState::Ambiguous {
            candidates: vec![
                crate::model::entity::EntityId::new(
                    crate::model::path::RepoPath::new("src/a.rs").expect("path"),
                    EntityKind::Method,
                    "A.render",
                    0,
                ),
                crate::model::entity::EntityId::new(
                    crate::model::path::RepoPath::new("src/b.rs").expect("path"),
                    EntityKind::Method,
                    "B.render",
                    0,
                ),
            ],
        })
        .expect("encode");
        assert!(!payload.contains("candidates"), "payload: {payload}");
        assert!(!payload.contains("A.render"), "payload: {payload}");
        assert!(payload.contains("ambiguous"), "payload: {payload}");
    }

    #[test]
    fn a_resolved_payload_keeps_its_evidence_including_the_alias() {
        let payload = resolution_payload(&ResolutionState::Resolved {
            by: Evidence::ImportBinding {
                module: "../payments".to_owned(),
                alias: Some("Svc".to_owned()),
            },
        })
        .expect("encode");
        assert!(payload.contains("import_binding"), "payload: {payload}");
        assert!(payload.contains("Svc"), "payload: {payload}");
    }

    #[test]
    fn an_unresolved_payload_keeps_its_reason() {
        let payload = resolution_payload(&ResolutionState::Unresolved {
            reason: UnresolvedReason::ParseError,
        })
        .expect("encode");
        assert!(payload.contains("parse_error"), "payload: {payload}");
    }

    #[test]
    fn a_payload_round_trips_to_the_same_state() {
        let states = [
            ResolutionState::Resolved {
                by: Evidence::QualifiedNameInScope {
                    scope: "src/payments".to_owned(),
                },
            },
            ResolutionState::Unresolved {
                reason: UnresolvedReason::ParseError,
            },
            ResolutionState::Inferred {
                by: Evidence::NameOnly,
                basis: "second-parity heuristic".to_owned(),
            },
        ];
        for state in states {
            let payload = resolution_payload(&state).expect("encode");
            let back: ResolutionState = serde_json::from_str(&payload).expect("decode");
            assert_eq!(back, state);
        }
    }

    #[test]
    fn only_the_ambiguous_state_is_treated_as_needing_candidates() {
        assert!(is_ambiguous(&ResolutionState::Ambiguous {
            candidates: vec![]
        }));
        assert!(!is_ambiguous(&ResolutionState::Resolved {
            by: Evidence::UniqueName
        }));
        assert!(!is_ambiguous(&ResolutionState::Unresolved {
            reason: UnresolvedReason::NoCandidate
        }));
        assert!(!is_ambiguous(&ResolutionState::Inferred {
            by: Evidence::NameOnly,
            basis: "x".to_owned()
        }));
    }

    #[test]
    fn a_stored_path_that_no_longer_validates_is_rejected() {
        assert!(path_from_sql("src/a.rs").is_ok());
        // A build with laxer normalisation could have written these. Accepting them would put an
        // entity outside the repository root into the graph.
        assert!(matches!(
            path_from_sql("../escape.rs"),
            Err(StoreError::Corrupt(_))
        ));
        assert!(matches!(path_from_sql(""), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn evidence_survives_a_payload_round_trip() {
        for evidence in [
            Evidence::ImportBinding {
                module: "../payments".to_owned(),
                alias: Some("Svc".to_owned()),
            },
            Evidence::ReceiverType {
                receiver: "PaymentService".to_owned(),
            },
            Evidence::NameOnly,
        ] {
            let text = serde_json::to_string(&evidence).expect("encode");
            let back: Evidence = serde_json::from_str(&text).expect("decode");
            assert_eq!(back, evidence);
        }
    }
}
