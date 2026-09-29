//! Relations between entities, and the evidence that justifies them.
//!
//! # The gap this module closes
//!
//! Cortex's `Edge` had a `reason: String` field. Every producer set it to a constant —
//! `"call"`, `"import"`, `"identifier"`, `"symbol definition"`, `"derived dependency"` — so it
//! recorded *which AST node triggered extraction* and nothing about *how the target was
//! resolved*. A call resolved because exactly one entity in the repository had that name and a
//! call resolved by `symbols.first()` were byte-identical records. There was no field on any
//! type that could have distinguished them.
//!
//! The consequence was not cosmetic. `explain()` counted inbound edges with no kind filter and
//! labelled the result "N callers", and because every symbol is guaranteed a `Defines` edge, that
//! number was off by at least one *always*. The engine could not tell the truth about its own
//! output, so it did not.
//!
//! Peek makes resolution a typed, queryable value. [`ResolutionState`] is carried on every
//! [`Relation`], [`Evidence`] is an enum rather than prose, and unresolved relations are
//! **persisted** rather than dropped at the first sign of ambiguity. There are deliberately no
//! numeric confidence scores: an uncalibrated percentage is worse than an honest label.
//!
//! The other half of the fix is that ambiguity is a *result*, never a silent pick. See
//! [`ResolutionState::Ambiguous`].

use std::fmt;

use serde::{Deserialize, Serialize};

use super::entity::EntityId;
use super::span::Span;

/// What kind of relationship two entities have.
///
/// Every variant here answers a question an agent actually asks. Vocabulary is added when a
/// real extractor can prove it, not because it sounds sophisticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    // ---- structural ----
    /// A file or module declares an entity.
    Defines,
    /// An entity lexically encloses another.
    Contains,
    /// An entity is a member of a type.
    Owns,

    // ---- module system ----
    Imports,
    Exports,
    Reexports,

    // ---- usage ----
    References,
    Calls,
    Reads,
    Writes,
    Mutates,

    // ---- types ----
    UsesType,
    Returns,
    Accepts,
    Throws,
    Constructs,
    Instantiates,

    // ---- hierarchies ----
    Inherits,
    Implements,
    Overrides,

    // ---- behavioural ----
    ConfiguredBy,
    TestedBy,
    RoutesTo,
    Handles,
    Publishes,
    Subscribes,
}

impl RelationKind {
    /// A stable, lowercase label for CLI and MCP output.
    pub fn as_str(self) -> &'static str {
        match self {
            RelationKind::Defines => "defines",
            RelationKind::Contains => "contains",
            RelationKind::Owns => "owns",
            RelationKind::Imports => "imports",
            RelationKind::Exports => "exports",
            RelationKind::Reexports => "reexports",
            RelationKind::References => "references",
            RelationKind::Calls => "calls",
            RelationKind::Reads => "reads",
            RelationKind::Writes => "writes",
            RelationKind::Mutates => "mutates",
            RelationKind::UsesType => "uses_type",
            RelationKind::Returns => "returns",
            RelationKind::Accepts => "accepts",
            RelationKind::Throws => "throws",
            RelationKind::Constructs => "constructs",
            RelationKind::Instantiates => "instantiates",
            RelationKind::Inherits => "inherits",
            RelationKind::Implements => "implements",
            RelationKind::Overrides => "overrides",
            RelationKind::ConfiguredBy => "configured_by",
            RelationKind::TestedBy => "tested_by",
            RelationKind::RoutesTo => "routes_to",
            RelationKind::Handles => "handles",
            RelationKind::Publishes => "publishes",
            RelationKind::Subscribes => "subscribes",
        }
    }

    /// Whether this relation is established purely by grammar and needs no name resolution.
    ///
    /// Structural relations are always [`ResolutionState::Resolved`], which lets consumers skip
    /// the ambiguity handling path for them.
    pub fn is_structural(self) -> bool {
        matches!(
            self,
            RelationKind::Defines | RelationKind::Contains | RelationKind::Owns
        )
    }

    /// Whether this relation implies a dependency direction, so that it participates in
    /// `dependencies` / `dependents` traversal.
    pub fn is_dependency(self) -> bool {
        !self.is_structural()
    }

    /// The kind a spelling names, or `None` for a spelling this build does not have.
    ///
    /// The inverse of [`RelationKind::as_str`], and the reason it lives here rather than in a
    /// caller. A surface that accepts a relation kind as text — a CLI flag, an MCP argument — has
    /// to decide what to do with a string that is not one, and the answer cannot be "no filter":
    /// audit D, section F, records the engine this replaces answering `--kind nonsense` with
    /// every kind and the caller reading it as a filtered result. Refusing is the only honest
    /// option, and this function is what makes refusing possible without a second list of the
    /// vocabulary to keep in step with the enum.
    pub fn parse(text: &str) -> Option<Self> {
        ALL_RELATION_KINDS
            .iter()
            .copied()
            .find(|kind| kind.as_str() == text)
    }
}

/// Every relation kind, in declaration order.
///
/// One list, owned by the enum's own module, so a caller that needs to offer the vocabulary cannot
/// fall out of step with it. The order is the order the variants are declared in — structural
/// first, then module, usage, types, hierarchy, behaviour — which is the order that reads as a
/// grouping rather than as an accident.
///
/// **A slice rather than a fixed-length array.** It was `[RelationKind; 27]` while holding 26
/// entries, and the type in the annotation is a number somebody has to keep in step with a list
/// that has to be kept in step with an enum. A hand-counted length is a third thing to maintain and
/// the only one of the three that fails to compile, so it is not worth carrying: `.len()` answers
/// the question it was there to answer, and a variant that reaches the list without reaching a
/// caller is caught by `the_listed_kinds_are_the_ones_the_enum_has`, which checks the actual
/// invariant rather than a count.
pub const ALL_RELATION_KINDS: &[RelationKind] = &[
    RelationKind::Defines,
    RelationKind::Contains,
    RelationKind::Owns,
    RelationKind::Imports,
    RelationKind::Exports,
    RelationKind::Reexports,
    RelationKind::References,
    RelationKind::Calls,
    RelationKind::Reads,
    RelationKind::Writes,
    RelationKind::Mutates,
    RelationKind::UsesType,
    RelationKind::Returns,
    RelationKind::Accepts,
    RelationKind::Throws,
    RelationKind::Constructs,
    RelationKind::Instantiates,
    RelationKind::Inherits,
    RelationKind::Implements,
    RelationKind::Overrides,
    RelationKind::ConfiguredBy,
    RelationKind::TestedBy,
    RelationKind::RoutesTo,
    RelationKind::Handles,
    RelationKind::Publishes,
    RelationKind::Subscribes,
];

/// The vocabulary as text, in the same order as [`ALL_RELATION_KINDS`].
///
/// For an error message that has to name every spelling rather than say "unknown kind", because a
/// caller who is told a kind is wrong and not what the right ones are will simply guess again.
#[must_use]
pub fn relation_kind_names() -> Vec<&'static str> {
    ALL_RELATION_KINDS
        .iter()
        .map(|kind| kind.as_str())
        .collect()
}

impl fmt::Display for RelationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a relation's target was established.
///
/// An enum, not a string. Two relations with different evidence are different values even when
/// they connect the same pair with the same kind, and the difference survives serialisation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Evidence {
    /// Grammatical containment: the enclosing node is the target. Always sound.
    Containment,
    /// An import binding in the referring file mapped this name to this entity. Strong: the
    /// author said where it came from.
    ImportBinding {
        /// The module path as written, e.g. `../payments/service`.
        module: String,
        /// The local alias, when the import renamed the symbol.
        alias: Option<String>,
    },
    /// A receiver expression whose type is statically known in the source language.
    ReceiverType {
        /// The receiver's type as written, e.g. `PaymentService`.
        receiver: String,
    },
    /// The qualified name resolves uniquely within the containing module or package.
    QualifiedNameInScope {
        /// The module or package the lookup was confined to.
        scope: String,
    },
    /// Exactly one entity in the whole repository carries this name. Sound, but weak evidence:
    /// uniqueness is not intent.
    UniqueName,
    /// The target is declared in the same file as the reference.
    SameFile,
    /// The target was identified by an explicit path.
    PathMatch,
    /// For second-parity languages where no stronger evidence is available. Explicitly marked
    /// so a consumer can discount it.
    NameOnly,
}

impl Evidence {
    /// A stable label naming the evidence class, without its payload.
    pub fn class(&self) -> &'static str {
        match self {
            Evidence::Containment => "containment",
            Evidence::ImportBinding { .. } => "import_binding",
            Evidence::ReceiverType { .. } => "receiver_type",
            Evidence::QualifiedNameInScope { .. } => "qualified_name_in_scope",
            Evidence::UniqueName => "unique_name",
            Evidence::SameFile => "same_file",
            Evidence::PathMatch => "path_match",
            Evidence::NameOnly => "name_only",
        }
    }

    /// How much this evidence constrains the answer.
    ///
    /// Ordered strongest first. Used only to *rank* competing candidates for presentation; it
    /// never silently promotes an edge to resolved.
    ///
    /// Takes `&self` because ranking is a read. `Evidence` carries owned strings and so cannot be
    /// `Copy`, and a by-value signature here means every comparison of two candidates has to
    /// clone both of them — or, worse, be written to move and then be unusable.
    pub fn strength(&self) -> u8 {
        match self {
            Evidence::Containment => 100,
            Evidence::ImportBinding { .. } => 90,
            Evidence::ReceiverType { .. } => 85,
            Evidence::PathMatch => 80,
            Evidence::QualifiedNameInScope { .. } => 70,
            Evidence::SameFile => 50,
            Evidence::UniqueName => 40,
            Evidence::NameOnly => 10,
        }
    }
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.class())
    }
}

/// Why a relation could not be resolved.
///
/// **The wire form is a bare string**, because every variant here is a unit variant and a unit
/// variant needs no tag: `{"state":"unresolved","reason":"external"}`. It used to be written as
/// `{"reason":{"reason":"external"}}`, because this enum carried `#[serde(tag = "reason")]` while
/// sitting inside a struct field *also* called `reason`. A private `UnresolvedReasonWire` accepts
/// both on the way in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "UnresolvedReasonWire")]
pub enum UnresolvedReason {
    /// The name was found but nothing in the repository matches it.
    NoCandidate,
    /// More than one candidate matched and none carried decisive evidence. This is a real,
    /// reportable ambiguity, not a failure.
    Ambiguous,
    /// The target lives outside the repository: a standard library, a third-party dependency, or
    /// a generated artefact that was not indexed.
    External,
    /// The callee is computed at runtime, so no static target exists.
    Dynamic,
    /// The file did not parse cleanly, so the surrounding construct is unreliable.
    ParseError,
    /// The language has no extractor rules that can prove this relation.
    Unsupported,
}

/// The two shapes an [`UnresolvedReason`] is found in on the way in.
///
/// **The wrapped shape is not obsolete data; it is what is on disk.** `ResolutionState` is
/// persisted: the store writes it to `resolution_json` and reads it back with `from_str`, so every
/// index built by an earlier release holds the wrapped form for its unresolved relations. Writing
/// one shape and refusing the other would turn correct data into `StoreError::Corrupt` and force
/// every index to be rebuilt to read a value that never changed meaning — so the deserialiser
/// accepts both and the serialiser writes one. A `SCHEMA_VERSION` bump would have been the other
/// answer, and it would have cost a migration and a rebuild for a change no reader needs.
///
/// A variant added to the enum needs no change here: [`UnresolvedReason::parse`] is the only place
/// a spelling is matched, and a spelling it does not know is an error rather than a default.
#[derive(Deserialize)]
#[serde(untagged)]
enum UnresolvedReasonWire {
    /// `{"reason":"external"}` — the shape every existing index holds.
    Wrapped { reason: String },
    /// `"external"` — the shape this build writes.
    Bare(String),
}

impl TryFrom<UnresolvedReasonWire> for UnresolvedReason {
    type Error = String;

    fn try_from(wire: UnresolvedReasonWire) -> Result<Self, Self::Error> {
        let text = match wire {
            UnresolvedReasonWire::Wrapped { reason } | UnresolvedReasonWire::Bare(reason) => reason,
        };
        Self::parse(&text).ok_or_else(|| {
            format!("{text:?} is not an unresolved reason this build knows, so refusing it rather \
                     than reading it as something else")
        })
    }
}

impl UnresolvedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnresolvedReason::NoCandidate => "no_candidate",
            UnresolvedReason::Ambiguous => "ambiguous",
            UnresolvedReason::External => "external",
            UnresolvedReason::Dynamic => "dynamic",
            UnresolvedReason::ParseError => "parse_error",
            UnresolvedReason::Unsupported => "unsupported",
        }
    }

    /// The reason a spelling names, or `None` for a spelling this build does not have.
    ///
    /// The inverse of [`Self::as_str`], and the reason it lives here rather than in a caller: a
    /// stored value this build cannot name is a corrupt row, and the only honest reading of one is
    /// to refuse it. Defaulting to the nearest known reason would turn a store written by a newer
    /// build into a confidently wrong answer.
    fn parse(text: &str) -> Option<Self> {
        match text {
            "no_candidate" => Some(UnresolvedReason::NoCandidate),
            "ambiguous" => Some(UnresolvedReason::Ambiguous),
            "external" => Some(UnresolvedReason::External),
            "dynamic" => Some(UnresolvedReason::Dynamic),
            "parse_error" => Some(UnresolvedReason::ParseError),
            "unsupported" => Some(UnresolvedReason::Unsupported),
            _ => None,
        }
    }
}

/// The resolution state of a relation.
///
/// This is the field Cortex did not have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ResolutionState {
    /// Extracted from source but not yet run through the resolver.
    ///
    /// Extraction and resolution are separate stages, and a relation has to cross between them
    /// carrying whatever evidence the extractor gathered: the module and alias of an import
    /// binding, the receiver text of a method call, the enclosing scope of a reference. Folding
    /// that evidence into a later stage would mean re-parsing the file; dropping it would mean
    /// guessing later. Cortex's central failure was precisely that `Edge.reason` was
    /// informationally empty, so this variant exists to make the hand-off impossible to lose.
    Pending {
        evidence: Evidence,
        /// How the relation was seen, in one sentence — "`impl` relation on `Stripe`". Carried
        /// here for the same reason [`ResolutionState::Inferred`] carries one: `peek explain`
        /// must be able to say *why* an edge exists without re-parsing the file. Folding it into
        /// the evidence enum would mean a new variant per syntax form, which is how a type turns
        /// into a taxonomy that has to be extended with every grammar.
        basis: String,
    },
    /// Proven. Exactly one target, with evidence that supports it.
    Resolved { by: Evidence },
    /// More than one candidate fits and none was proven correct. **The relation is reported, not
    /// guessed.** An agent can disambiguate and ask again.
    Ambiguous {
        /// Candidate targets, ordered strongest evidence first.
        candidates: Vec<EntityId>,
    },
    /// No target could be established.
    Unresolved { reason: UnresolvedReason },
    /// Derived from another fact rather than directly observed in source.
    Inferred {
        by: Evidence,
        /// What this was inferred from, in words the consumer can audit.
        basis: String,
    },
}

impl ResolutionState {
    /// Whether the relation has exactly one proven target.
    pub fn is_resolved(&self) -> bool {
        matches!(
            self,
            ResolutionState::Resolved { .. } | ResolutionState::Inferred { .. }
        )
    }

    /// Whether the relation has been extracted but not yet resolved.
    pub fn is_pending(&self) -> bool {
        matches!(self, ResolutionState::Pending { .. })
    }

    /// Whether a consumer must handle the possibility of several targets.
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, ResolutionState::Ambiguous { .. })
    }

    /// Whether the relation is explicitly known to be unresolved, and why.
    pub fn is_unresolved(&self) -> bool {
        matches!(self, ResolutionState::Unresolved { .. })
    }

    /// The evidence class, when there is any.
    pub fn evidence_class(&self) -> Option<&'static str> {
        match self {
            ResolutionState::Pending { evidence, .. }
            | ResolutionState::Resolved { by: evidence }
            | ResolutionState::Inferred { by: evidence, .. } => Some(evidence.class()),
            ResolutionState::Ambiguous { .. } | ResolutionState::Unresolved { .. } => None,
        }
    }

    /// A one-line, human-readable rendering for CLI and MCP output.
    pub fn describe(&self) -> String {
        match self {
            ResolutionState::Pending { evidence, basis } => {
                format!("pending ({evidence}): {basis}")
            }
            ResolutionState::Resolved { by } => format!("resolved ({by})"),
            ResolutionState::Inferred { by, basis } => format!("inferred ({by}): {basis}"),
            ResolutionState::Ambiguous { candidates } => {
                if candidates.is_empty() {
                    "ambiguous (no candidates)".to_owned()
                } else {
                    format!("ambiguous ({} candidates)", candidates.len())
                }
            }
            ResolutionState::Unresolved { reason } => format!("unresolved ({})", reason.as_str()),
        }
    }
}

/// A relationship between a source entity and a named target.
///
/// `target_name` always holds the text as it was written, so provenance survives even when the
/// target could not be resolved. `target` holds the entity only when resolution succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub kind: RelationKind,
    /// The entity the relation originates from.
    pub source: EntityId,
    /// The target exactly as written in source. Never empty for a resolved relation.
    pub target_name: String,
    /// The resolved target entity, present only when resolution succeeded.
    pub target: Option<EntityId>,
    /// Where this relation appears in source.
    pub span: Span,
    /// How the target was established, or why it could not be.
    pub resolution: ResolutionState,
}

impl Relation {
    /// A relation freshly extracted from source, carrying the evidence the extractor found but
    /// not yet resolved to a target.
    pub fn pending(
        kind: RelationKind,
        source: EntityId,
        target_name: impl Into<String>,
        span: Span,
        evidence: Evidence,
        basis: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            source,
            target_name: target_name.into(),
            target: None,
            span,
            resolution: ResolutionState::Pending {
                evidence,
                basis: basis.into(),
            },
        }
    }

    /// A relation whose target is proven.
    pub fn resolved(
        kind: RelationKind,
        source: EntityId,
        target: EntityId,
        target_name: impl Into<String>,
        span: Span,
        by: Evidence,
    ) -> Self {
        Self {
            kind,
            source,
            target_name: target_name.into(),
            target: Some(target),
            span,
            resolution: ResolutionState::Resolved { by },
        }
    }

    /// A relation whose target could not be established. Persisted, never discarded.
    pub fn unresolved(
        kind: RelationKind,
        source: EntityId,
        target_name: impl Into<String>,
        span: Span,
        reason: UnresolvedReason,
    ) -> Self {
        Self {
            kind,
            source,
            target_name: target_name.into(),
            target: None,
            span,
            resolution: ResolutionState::Unresolved { reason },
        }
    }

    /// A relation with several equally plausible targets and no decisive evidence.
    pub fn ambiguous(
        kind: RelationKind,
        source: EntityId,
        target_name: impl Into<String>,
        span: Span,
        candidates: Vec<EntityId>,
    ) -> Self {
        Self {
            kind,
            source,
            target_name: target_name.into(),
            target: None,
            span,
            resolution: ResolutionState::Ambiguous { candidates },
        }
    }

    /// A relation derived from another fact.
    pub fn inferred(
        kind: RelationKind,
        source: EntityId,
        target: EntityId,
        target_name: impl Into<String>,
        span: Span,
        by: Evidence,
        basis: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            source,
            target_name: target_name.into(),
            target: Some(target),
            span,
            resolution: ResolutionState::Inferred {
                by,
                basis: basis.into(),
            },
        }
    }

    /// Whether this relation can be followed to a single target.
    pub fn is_followable(&self) -> bool {
        self.resolution.is_resolved() && self.target.is_some()
    }

    /// The twelve values that make two records the same relation.
    ///
    /// This is the same natural key the store's `UNIQUE` constraint uses, spelled once here so a
    /// caller de-duplicating relations in memory cannot quietly disagree with the database about
    /// what "the same relation" means. The disagreement is not hypothetical: the write path
    /// already had one copy of this list, and a second copy that drifted would make a caller
    /// believe it had found two relations where the store has one.
    #[must_use]
    pub fn natural_key(&self) -> RelationKey {
        RelationKey {
            source_path: self.source.path().as_str().to_owned(),
            source_kind: self.source.kind().as_str().to_owned(),
            source_qualified_name: self.source.qualified_name().to_owned(),
            source_ordinal: self.source.ordinal(),
            kind: self.kind.as_str().to_owned(),
            target_name: self.target_name.clone(),
            start_byte: self.span.start_byte,
            end_byte: self.span.end_byte,
            start_line: self.span.start_line,
            start_column: self.span.start_column,
            end_line: self.span.end_line,
            end_column: self.span.end_column,
        }
    }
}

/// The identity of a relation: who said it, what they said, to what, and where.
///
/// Not `EntityId`, because a relation is not an entity and has no name of its own. Derived rather
/// than stored, so it cannot disagree with the row it describes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelationKey {
    pub source_path: String,
    pub source_kind: String,
    pub source_qualified_name: String,
    pub source_ordinal: u32,
    pub kind: String,
    pub target_name: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[cfg(test)]
mod tests {
    use super::{
        ALL_RELATION_KINDS, Evidence, Relation, RelationKind, ResolutionState, UnresolvedReason,
        relation_kind_names,
    };
    use crate::model::entity::{EntityId, EntityKind};
    use crate::model::path::RepoPath;
    use crate::model::span::Span;

    fn id(qname: &str) -> EntityId {
        EntityId::new(
            RepoPath::new("src/a.rs").expect("path"),
            EntityKind::Function,
            qname,
            0,
        )
    }

    fn span() -> Span {
        Span::new(0, 10, 1, 1, 1, 11).expect("span")
    }

    #[test]
    fn evidence_is_typed_not_prose() {
        // The two relations below differ only in how their target was established. Under Cortex's
        // `reason: String` these were byte-identical records.
        let unique = Relation::resolved(
            RelationKind::Calls,
            id("main"),
            id("helper"),
            "helper",
            span(),
            Evidence::UniqueName,
        );
        let by_import = Relation::resolved(
            RelationKind::Calls,
            id("main"),
            id("helper"),
            "helper",
            span(),
            Evidence::ImportBinding {
                module: "./helper".to_owned(),
                alias: None,
            },
        );

        assert_ne!(unique.resolution, by_import.resolution);
        assert_eq!(
            unique.resolution.evidence_class(),
            Some("unique_name"),
            "a globally-unique name is weaker evidence than an explicit import"
        );
        assert_eq!(
            by_import.resolution.evidence_class(),
            Some("import_binding")
        );
        assert!(
            Evidence::ImportBinding {
                module: "m".to_owned(),
                alias: None
            }
            .strength()
                > Evidence::UniqueName.strength()
        );
    }

    #[test]
    fn ambiguity_is_persisted_not_guessed() {
        // `symbols.first()` is banned. Ambiguity must survive to the consumer.
        let relation = Relation::ambiguous(
            RelationKind::Calls,
            id("main"),
            "render",
            span(),
            vec![id("A.render"), id("B.render")],
        );

        assert!(relation.target.is_none());
        assert!(relation.resolution.is_ambiguous());
        assert!(!relation.resolution.is_resolved());
        assert!(!relation.is_followable());
        assert_eq!(relation.resolution.describe(), "ambiguous (2 candidates)");
        // The name as written is preserved regardless of resolution outcome.
        assert_eq!(relation.target_name, "render");
    }

    #[test]
    fn unresolved_relations_are_retained_with_a_reason() {
        // Cortex did `else { continue }` here, so a relation that resolved to nothing left
        // no trace at all. It has to be countable now.
        let external = Relation::unresolved(
            RelationKind::Calls,
            id("main"),
            "printf",
            span(),
            UnresolvedReason::External,
        );
        assert!(external.resolution.is_unresolved());
        assert!(external.target.is_none());
        assert_eq!(external.resolution.describe(), "unresolved (external)");

        let dynamic = Relation::unresolved(
            RelationKind::Calls,
            id("main"),
            "<computed>",
            span(),
            UnresolvedReason::Dynamic,
        );
        assert_eq!(dynamic.resolution.describe(), "unresolved (dynamic)");
    }

    #[test]
    fn resolved_relations_are_followable() {
        let relation = Relation::resolved(
            RelationKind::Calls,
            id("main"),
            id("helper"),
            "helper",
            span(),
            Evidence::SameFile,
        );
        assert!(relation.is_followable());
        assert_eq!(relation.target.as_ref().map(EntityId::name), Some("helper"));
    }

    #[test]
    fn inferred_relations_carry_their_basis() {
        let relation = Relation::inferred(
            RelationKind::Implements,
            id("StripeGateway"),
            id("PaymentGateway"),
            "PaymentGateway",
            span(),
            Evidence::QualifiedNameInScope {
                scope: "src/payments".to_owned(),
            },
            "the only PaymentGateway trait in the enclosing module",
        );
        assert!(relation.is_followable());
        assert!(relation.resolution.is_resolved());
        assert!(relation.resolution.describe().contains("inferred"));
        assert!(
            relation
                .resolution
                .describe()
                .contains("only PaymentGateway")
        );
    }

    #[test]
    fn structural_relations_are_flagged() {
        assert!(RelationKind::Defines.is_structural());
        assert!(RelationKind::Contains.is_structural());
        assert!(RelationKind::Owns.is_structural());
        assert!(!RelationKind::Calls.is_structural());

        assert!(RelationKind::Calls.is_dependency());
        assert!(!RelationKind::Contains.is_dependency());
    }

    #[test]
    fn resolution_states_round_trip_through_serde() {
        // The distinction only matters if it survives persistence, which is where Cortex lost it.
        let cases = vec![
            ResolutionState::Pending {
                evidence: Evidence::ImportBinding {
                    module: "../payments".to_owned(),
                    alias: Some("Svc".to_owned()),
                },
                basis: "import of `payments::Svc`".to_owned(),
            },
            ResolutionState::Resolved {
                by: Evidence::ImportBinding {
                    module: "../payments".to_owned(),
                    alias: Some("Svc".to_owned()),
                },
            },
            ResolutionState::Ambiguous {
                candidates: vec![id("A.render")],
            },
            ResolutionState::Unresolved {
                reason: UnresolvedReason::ParseError,
            },
            ResolutionState::Inferred {
                by: Evidence::NameOnly,
                basis: "second-parity heuristic".to_owned(),
            },
        ];

        for state in cases {
            let json = serde_json::to_string(&state).expect("serialise");
            let back: ResolutionState = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, state, "round trip changed the resolution state");
        }
    }

    #[test]
    fn an_unresolved_reason_travels_as_a_bare_string() {
        // Every `UnresolvedReason` variant is a unit variant, so there is nothing for a tag to
        // carry, and the field it sits in is already called `reason`. The wire form is therefore
        // `"reason":"external"` rather than `"reason":{"reason":"external"}` — one vocabulary word
        // where the reader's model says there is one.
        let state = ResolutionState::Unresolved {
            reason: UnresolvedReason::External,
        };
        assert_eq!(
            serde_json::to_string(&state).expect("serialise"),
            r#"{"state":"unresolved","reason":"external"}"#
        );
    }

    #[test]
    fn a_resolution_state_stored_before_the_flat_reason_is_still_read() {
        // The reason the deserialiser accepts two shapes rather than one. These are literals: the
        // exact bytes every index built before the change holds in its `resolution_json` column,
        // the first of them the stand-in written for *every* ambiguous relation in every index.
        // Nothing rebuilds and nothing migrates — an index written by the old build is simply still
        // readable, because the data was never wrong.
        let stored = [
            (
                r#"{"state":"unresolved","reason":{"reason":"ambiguous"}}"#,
                UnresolvedReason::Ambiguous,
            ),
            (
                r#"{"state":"unresolved","reason":{"reason":"external"}}"#,
                UnresolvedReason::External,
            ),
            (
                r#"{"state":"unresolved","reason":{"reason":"no_candidate"}}"#,
                UnresolvedReason::NoCandidate,
            ),
        ];
        for (payload, expected) in stored {
            let read: ResolutionState = serde_json::from_str(payload)
                .unwrap_or_else(|error| panic!("{payload} must still decode: {error}"));
            assert_eq!(
                read,
                ResolutionState::Unresolved { reason: expected },
                "{payload} decoded to the wrong reason"
            );
        }
    }

    #[test]
    fn both_shapes_of_a_reason_are_the_same_value() {
        // The pair, side by side: what the store holds and what this build writes name the same
        // reason. Without this the two above only say that each shape decodes, not that the
        // change was a change of spelling rather than of meaning.
        let wrapped: UnresolvedReason =
            serde_json::from_str(r#"{"reason":"external"}"#).expect("the stored shape");
        let bare: UnresolvedReason =
            serde_json::from_str(r#""external""#).expect("the written shape");
        assert_eq!(wrapped, bare);
        assert_eq!(serde_json::to_string(&bare).expect("serialise"), r#""external""#);
    }

    #[test]
    fn a_reason_spelling_this_build_does_not_have_is_refused_rather_than_guessed() {
        // A row written by a newer build, or a corrupted one. Substituting the nearest known
        // reason would report a resolution claim the file never made.
        let outcome: Result<UnresolvedReason, _> = serde_json::from_str(r#""maybe""#);
        assert!(outcome.is_err(), "an unknown reason must not decode");
        let nested: Result<ResolutionState, _> =
            serde_json::from_str(r#"{"state":"unresolved","reason":{"reason":"maybe"}}"#);
        assert!(
            nested.is_err(),
            "an unknown reason inside the stored shape must not decode either"
        );
    }

    #[test]
    fn parsing_a_reason_spelling_inverts_printing_one() {
        // `parse` is the only place the two spellings meet, so the pair is pinned here rather
        // than left to a reader: a variant added without a spelling is a value that cannot be
        // read back out of a row it was just written into.
        let every = [
            UnresolvedReason::NoCandidate,
            UnresolvedReason::Ambiguous,
            UnresolvedReason::External,
            UnresolvedReason::Dynamic,
            UnresolvedReason::ParseError,
            UnresolvedReason::Unsupported,
        ];
        for reason in every {
            let spelled = reason.as_str();
            assert_eq!(
                UnresolvedReason::parse(spelled),
                Some(reason.clone()),
                "{spelled} did not parse back to the reason it was printed from"
            );
        }
        assert_eq!(UnresolvedReason::parse("External"), None);
        assert_eq!(UnresolvedReason::parse(""), None);
    }

    #[test]
    fn pending_and_resolved_are_distinguishable_after_round_trip() {
        // The extractor→resolver hand-off is only safe if "extracted but unresolved" survives
        // persistence as something other than "resolved". Cortex's `reason` string made these
        // two records byte-identical, which is why it could never tell them apart.
        let evidence = Evidence::ReceiverType {
            receiver: "PaymentService".to_owned(),
        };
        let pending = Relation::pending(
            RelationKind::Calls,
            id("main"),
            "retry",
            span(),
            evidence.clone(),
            "call to `retry`",
        );
        let resolved = Relation::resolved(
            RelationKind::Calls,
            id("main"),
            id("retry"),
            "retry",
            span(),
            evidence,
        );

        assert!(pending.resolution.is_pending());
        assert!(!pending.resolution.is_resolved());
        assert!(!pending.is_followable());
        assert!(resolved.resolution.is_resolved());
        assert!(!resolved.resolution.is_pending());

        let json = serde_json::to_string(&pending).expect("serialise");
        let back: Relation = serde_json::from_str(&json).expect("deserialise");
        assert!(back.resolution.is_pending());
        assert_eq!(back.resolution.evidence_class(), Some("receiver_type"));
    }

    #[test]
    fn pending_carries_the_extractor_evidence() {
        // An import binding's module and alias must survive to the resolver. Cortex's
        // `ExtractedRelation` had no alias field at all, so the mapping was destroyed at
        // extraction time and the resolver had nothing to work with.
        let relation = Relation::pending(
            RelationKind::Imports,
            id("main"),
            "Svc",
            span(),
            Evidence::ImportBinding {
                module: "../payments/service".to_owned(),
                alias: None,
            },
            "import of `Svc`",
        );
        assert!(relation.target.is_none());
        assert_eq!(relation.target_name, "Svc");
        match &relation.resolution {
            ResolutionState::Pending { evidence, .. } => match evidence {
                Evidence::ImportBinding { module, alias } => {
                    assert_eq!(module, "../payments/service");
                    assert!(alias.is_none());
                }
                other => panic!("expected an import binding, got {other:?}"),
            },
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[test]
    fn the_natural_key_ignores_the_target_and_the_resolution() {
        // Two records that agree on the natural key are the same relation however differently
        // they were resolved, because the store's `UNIQUE` constraint collapses them. A key that
        // included the target would let a re-resolution insert a *second* row for one relation,
        // and an index that doubles its edges on every resolve is not an index.
        let source = id("main");
        let target = id("helper");
        let pending = Relation::pending(
            RelationKind::Calls,
            source.clone(),
            "helper",
            span(),
            Evidence::NameOnly,
            "call to `helper`",
        );
        let decided = Relation::inferred(
            RelationKind::Calls,
            source,
            target,
            "helper",
            span(),
            Evidence::UniqueName,
            "the only `helper` indexed",
        );

        assert_eq!(
            pending.natural_key(),
            decided.natural_key(),
            "a re-decided relation must keep the identity the store already gave it"
        );
    }

    #[test]
    fn the_natural_key_separates_two_relations_from_the_same_source() {
        // The span is part of the key for a reason: a function that calls `helper` twice is two
        // references, and collapsing them would halve the call count of every function in the
        // repository that is called more than once.
        let source = id("main");
        let first = Relation::pending(
            RelationKind::Calls,
            source.clone(),
            "helper",
            Span::new(0, 10, 1, 1, 1, 11).expect("span"),
            Evidence::NameOnly,
            "call to `helper`",
        );
        let second = Relation::pending(
            RelationKind::Calls,
            source,
            "helper",
            Span::new(20, 30, 2, 5, 2, 15).expect("span"),
            Evidence::NameOnly,
            "call to `helper`",
        );
        assert_ne!(
            first.natural_key(),
            second.natural_key(),
            "two calls at different places are two relations"
        );
    }

    #[test]
    fn every_relation_kind_has_a_unique_label() {
        let mut labels: Vec<&str> = ALL_RELATION_KINDS.iter().map(|k| k.as_str()).collect();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count, "relation kind labels must be unique");
    }

    #[test]
    fn the_listed_kinds_are_the_ones_the_enum_has() {
        // `ALL_RELATION_KINDS` is what a surface offers as its vocabulary. If a kind is added to
        // the enum and not to the list, a caller is silently unable to ask for it — the same shape
        // of defect as the predecessor's `--kind nonsense` becoming "no filter", one level up.
        //
        // This used to assert `ALL_RELATION_KINDS.len() == 27`, with a comment saying the literal
        // was deliberate so that adding a variant without updating the list would fail here. It
        // would not have. The literal said nothing about which kinds were listed, so a variant
        // added to the enum and missed from the list left it passing — and the annotation said 27
        // while the list held 26, so the array length and the assertion agreed with each other and
        // disagreed with reality. Neither was reachable: the crate had never been built.
        //
        // The range is derived from the enum rather than written out. `RelationKind` is a fieldless
        // enum, so a cast is its discriminant, and the last variant's discriminant is one past the
        // last. Adding a variant above the last therefore widens the range and is checked without
        // anything here being edited; adding one below it is a compile error, because the
        // discriminants are the enum's and inserting a variant renumbers them.
        let listed: std::collections::BTreeSet<usize> = ALL_RELATION_KINDS
            .iter()
            .map(|kind| *kind as usize)
            .collect();
        let highest = RelationKind::Subscribes as usize;
        for discriminant in 0..=highest {
            assert!(
                listed.contains(&discriminant),
                "a relation kind the enum declares is missing from ALL_RELATION_KINDS, so no \
                 surface can ask for it: discriminant {discriminant}"
            );
        }
        assert_eq!(
            listed.len(),
            highest + 1,
            "ALL_RELATION_KINDS must list every kind once, with no duplicates and no extras"
        );
    }

    #[test]
    fn parsing_a_kind_name_inverts_printing_one() {
        // A surface that accepts a kind as text has to refuse an unknown one, and refusing needs
        // this to be a true inverse rather than a second list that can drift from the enum.
        for kind in ALL_RELATION_KINDS.iter().copied() {
            let spelled = kind.as_str();
            assert_eq!(
                RelationKind::parse(spelled),
                Some(kind),
                "`{spelled}` did not parse back to the kind it was printed from"
            );
        }
        assert_eq!(RelationKind::parse("nonsense"), None);
        assert_eq!(
            RelationKind::parse("Calls"),
            None,
            "the spelling is lowercase"
        );
        assert_eq!(relation_kind_names().len(), ALL_RELATION_KINDS.len());
    }
}
