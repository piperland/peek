//! The resolver: it turns `Pending` relations into decided ones.
//!
//! The engine Peek replaces had no resolution layer at all. Its indexer held a repo-global
//! `HashMap` from a name to symbol ids, tried three lookups against it, and took
//! `symbols.first()` when there was more than one match (audit B1, B3, B23). Two things followed,
//! and both were invisible in the output:
//!
//! * A call bound by an author-written import and a call bound by alphabetical order across the
//!   whole repository produced **byte-identical edges**, because `Edge.reason` was a constant
//!   string. No field on any type could hold the difference, so `explain()` printed prose it
//!   could not support (audit B10, B11).
//! * Every relation that resolved to nothing hit `let Some(id) = … else { continue }` and left
//!   no row, no counter and no log (audit B9). The gap between "Peek believes this" and "Peek
//!   has no idea" was the size of the third-party dependency tree, reported as zero.
//!
//! This module is the structural answer to both. Every rule that fires is named, every decision
//! records the evidence class that justified it, and every relation the resolver cannot decide is
//! written back as an `Unresolved` or an `Ambiguous` with its candidate list intact.
//!
//! # When resolution runs: a second pass, always
//!
//! **Decision: resolution is a follow-up batch, never an inline step of extraction, and it is
//! scoped to what changed.** `indexer::build_full` commits the extractor's output and then calls
//! [`resolve_all`]; `indexer::refresh` commits the changed files and then calls [`resolve_paths`]
//! over exactly those paths plus the edges that point into them. [`ResolutionReport`] is carried
//! on [`crate::indexer::IndexReport`] and named in its summary, so the choice is observable from
//! `peek status` and not only from this file.
//!
//! Four reasons, in the order they mattered:
//!
//! 1. **A decision needs the committed state, not the in-memory one.** Resolution is a function
//!    of the index as a whole: two files declaring `charge` make each other's edges ambiguous.
//!    Running it over the extractor's output before the commit would let it see a half-written
//!    repository and answer questions about it.
//! 2. **The two stages fail for different reasons and must be separately observable.** An
//!    extractor that mis-parses and a resolver that over-reaches are different bugs with
//!    different fixes. Two commits and two reports are what make them separable, and the
//!    generation advances twice so a reader can see that a resolution pass happened.
//! 3. **`refresh` over the changed paths is not sufficient on its own.** Moving `charge` from
//!    `a.rs` to `b.rs` changes every edge that pointed at it while leaving the sources of those
//!    edges untouched. [`resolve_paths`] therefore re-reads the *incoming* edges of every entity
//!    in the changed paths and decides them again. That is contract G9, and it is why the second
//!    pass is scoped by target as well as by source.
//! 4. **A crash between the two commits is honest rather than corrupt.** The index is a complete,
//!    readable generation whose relations are honestly `Pending` — countable, queryable through
//!    `relation_by_state`, and re-decidable by a later pass. Nothing claims to be resolved that
//!    is not.
//!
//! The cost is one extra commit per build and one extra pass of reads. Folding resolution into
//! the extractor's loop saves that and gives up all four properties.
//!
//! # The evidence order
//!
//! `Evidence::strength()` in `model::relation` is the total order and this module is written
//! against it. It is repeated here because `peek explain` prints it, and a reader of that output
//! has to be able to check it against something.
//!
//! | Strength | Class | What it means | Rung |
//! |---:|---|---|---|
//! | 100 | `containment` | grammatical nesting; the enclosing node *is* the target | not resolved here — structural relations are already `Resolved` at extraction |
//! | 90 | `import_binding` | the author wrote an import in this file naming this symbol | **R1** |
//! | 85 | `receiver_type` | a receiver expression whose owner is identified in this file | **R2** |
//! | 80 | `path_match` | the target was named by an explicit path | not used by this module yet |
//! | 70 | `qualified_name_in_scope` | a multi-segment path naming a module this file can locate | **R3** |
//! | 50 | `same_file` | the target is declared in the same file as the reference | **R4** |
//! | 40 | `unique_name` | exactly one entity in the repository carries the name | **R5** |
//! | 10 | `name_only` | a bare name with nothing else known about it | evidence the extractor records, never an answer |
//!
//! The order *is* the rung order. Rungs are tried strongest first and the **first rung that
//! returns any candidate decides**; a rung that returns none falls through to the next. A rung
//! that returns *several* does not fall through — several equally supported candidates are an
//! `Ambiguous`, which is a result and never a pick (D-0004). `symbols.first()` does not appear
//! anywhere in this file.
//!
//! # What each rung does, and what it refuses to do
//!
//! ## R1 — import binding
//!
//! The only rung stronger than a receiver, and for one reason: the author wrote it down. It fires
//! when the relation's `target_name` matches a local name bound by an `Imports` relation **in the
//! same file**, which is what lets a bare `charge()` in a file containing `use payments::charge;`
//! resolve to another file's definition.
//!
//! It is deliberately skipped when the relation carries `ReceiverType` evidence. A receiver and
//! an import of the same name are two claims about one symbol; when they disagree the receiver is
//! the more specific one, and letting the import win would re-create audit B3 under a new name.
//!
//! Turning a module path into a file is [`files_for_module`], and its limits are documented
//! there because they are real.
//!
//! ## R2 — receiver
//!
//! **A receiver is evidence, not a target.** `self.foo()` and `payments::Service::charge()` are
//! different facts and must not land on the same node.
//!
//! R2 finds the *owner* — the type the receiver names — and then accepts only entities declared
//! inside that owner. `self`, `this` and `Self` take their owner from the source's own qualified
//! name, which is exact. A named receiver is matched against the entities declared in the
//! referring file, because that is where a type the receiver names is declared in every language
//! this engine supports.
//!
//! **R2 never falls through.** If the owner cannot be identified, or nothing inside it carries
//! the name, the answer is `Unresolved`. Falling through to R4 or R5 would bind `service.retry()`
//! to whatever free function named `retry` sorts first in the repository, which is the single
//! worst behaviour the engine Peek replaces had.
//!
//! ## R3 — qualified name in scope
//!
//! A multi-segment path such as `crate::payments::Service::charge` names a module this file can
//! locate, and R3 searches that module's files for the name. A single-segment path never reaches
//! here: the extractor already reports `S::new()` as ambiguous, because `S` may be a type or a
//! module and nothing in the syntax distinguishes them.
//!
//! ## R4 — same file
//!
//! The target is declared in the file the reference appears in. For the reference kinds the
//! extractor emits this is the strong claim it sounds like, and it is the rung that makes a
//! single-file call resolve.
//!
//! ## R5 — unique name
//!
//! Exactly one entity in the repository carries the name. That is a fact about the index, not an
//! intent, so it produces [`ResolutionState::Inferred`] and never [`ResolutionState::Resolved`].
//! Two or more matches is `Ambiguous` — the case the engine Peek replaces resolved by taking the
//! alphabetically first file in the repository.
//!
//! R5 is the only rung that reaches outside the source file. It exists because the alternative is
//! a large class of honest `Unresolved`, and it is safe precisely because it can only fire when
//! the name is genuinely unique. It never picks between candidates.
//!
//! # `Resolved` and `Inferred` are different claims
//!
//! A target bound by an import binding, by an identified receiver, by a located module, or by a
//! definition in the same file is [`ResolutionState::Resolved`]: the target is proven. A target
//! bound by a repository-wide uniqueness check, or by a receiver matched while ignoring letter
//! case, is [`ResolutionState::Inferred`]: the target is a claim, and the relation's `basis` says
//! which claim, in words a consumer can audit.
//!
//! This is also the answer to "where did the case-insensitive fallback go". There is none, and
//! the reason is the interesting part: a case-insensitive lookup needs an index the store does not
//! have, and the only way to get one without adding a query path is to scan every entity in the
//! repository — the flat global name table this module exists to replace. So every name
//! comparison in this file is case-**sensitive**, with one narrow and visible exception: R2 may
//! match a receiver against an in-file type ignoring case, and when it does it says so in the
//! `basis` and records `Inferred` rather than `Resolved`. An unproven match is never reported as
//! a proof.
//!
//! # One transaction per pass
//!
//! A pass builds one [`IndexUpdate`] and hands it to [`Store::apply_update`] exactly once. A
//! failure at any point rolls the batch back and the previous generation stays readable with its
//! previous decisions intact. A pass that decides nothing does not commit at all, so
//! re-resolving an unchanged index costs a read and changes nothing.
//!
//! # Limits are reported, never silent
//!
//! Every lookup through the store is bounded, because an unbounded one is how the predecessor's
//! `resolve_target_id` became the first line of all ten of its queries. When a limit truncates a
//! candidate set the pass records it in [`ResolutionReport::truncated`], because a limit that is
//! not visible is a limit that quietly changes the answer.
//!
//! # What a scoped pass cannot see
//!
//! [`resolve_paths`] re-reads the outgoing edges of the files it was given and the incoming
//! edges of the entities they declare. That covers the two cases a refresh creates: a new or
//! changed edge out of a changed file, and an edge that used to point into a changed file.
//!
//! It does **not** cover a third: an `Ambiguous` or `Unresolved` edge elsewhere in the index
//! whose *candidate set* just changed, because a file appeared or an unrelated one was deleted.
//! Such an edge has no `target_path`, so `Store::incoming` cannot match it, and there is no
//! index from a target *name* back to the relations that name it. Closing that would need a new
//! index — a schema change — or a full re-resolve on every edit. Both are worse than the gap, so
//! the gap is documented here and pinned by a test rather than papered over. A full rebuild does
//! correct it: the extractor re-emits the edge as `Pending` and the pass decides it again.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::entity::{EntityId, EntityKind};
use crate::model::path::RepoPath;
use crate::model::relation::{
    Evidence, Relation, RelationKey, RelationKind, ResolutionState, UnresolvedReason,
};
use crate::store::{IndexUpdate, Store, StoreError};

#[cfg(test)]
mod tests;

/// The local name a glob import carries. `use a::b::*;` binds no single name, so no rule can
/// ever prove a target for it.
const GLOB: &str = "*";

/// How many ancestor directories a module path is anchored at.
///
/// The anchor list is the referring file's own directory and then each directory above it, up to
/// the repository root. A repository deeper than this needs a module table, not a longer scan;
/// the limit is what keeps the anchor list bounded.
const MAX_ANCHOR_DEPTH: usize = 6;

/// The longest module path a rule will act on. A path longer than this is not a module
/// reference; treating one as such would multiply the file seeks without bound.
const MAX_MODULE_SEGMENTS: usize = 8;

/// Every bounded lookup the resolver makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionOptions {
    /// Entities read from one file before the read is abandoned.
    pub entities_per_file: usize,
    /// Entities read for one name before the read is abandoned.
    pub entities_by_name: usize,
    /// Relations read from one source before the read is abandoned.
    pub outgoing_per_source: usize,
    /// Relations read into one target before the read is abandoned.
    pub incoming_per_entity: usize,
    /// Candidates written into one `Ambiguous` before the rest are dropped.
    pub max_candidates: usize,
    /// Re-decide relations that were already decided because their target is in scope.
    ///
    /// This is the "a definition moved" case, and it is on by default. Without it a move that
    /// makes a previously unique name ambiguous would leave a confidently-wrong `Inferred` edge
    /// in place indefinitely. Honoured by [`resolve_paths`] only; [`resolve_all`] ignores it,
    /// because a full build has re-emitted every relation as `Pending` already.
    pub reconsider_decided: bool,
}

impl Default for ResolutionOptions {
    fn default() -> Self {
        Self {
            entities_per_file: 512,
            entities_by_name: 512,
            outgoing_per_source: 512,
            incoming_per_entity: 512,
            max_candidates: 32,
            reconsider_decided: true,
        }
    }
}

impl ResolutionOptions {
    /// Set the per-file entity read limit. Chaining, so a test does not have to name the field.
    #[must_use]
    pub fn with_entities_per_file(mut self, limit: usize) -> Self {
        self.entities_per_file = limit;
        self
    }

    /// Set the per-name entity read limit.
    #[must_use]
    pub fn with_entities_by_name(mut self, limit: usize) -> Self {
        self.entities_by_name = limit;
        self
    }

    /// Set the candidate-list cap.
    #[must_use]
    pub fn with_max_candidates(mut self, limit: usize) -> Self {
        self.max_candidates = limit;
        self
    }

    /// Turn the "a definition moved" re-decision on or off.
    #[must_use]
    pub fn with_reconsider_decided(mut self, reconsider: bool) -> Self {
        self.reconsider_decided = reconsider;
        self
    }
}

/// The name of the rule a decision was made by, as it is stored.
///
/// A `Resolved` relation carries its rule in this class; an `Inferred` relation carries the class
/// *and* spells the claim out in its `basis`. `peek explain` reads this function to answer "why
/// do you think this calls that" without re-parsing the file, which is the capability Cortex's
/// `explain()` advertised and could not honestly deliver (audit B11).
#[must_use]
pub fn rule_name(evidence: &Evidence) -> &'static str {
    evidence.class()
}

/// The ladder rung a given evidence class belongs to.
///
/// Separate from [`rule_name`] because the rung and the evidence class are not the same
/// vocabulary: R2 records `receiver_type` but is the *receiver owner* rule. The mapping is total
/// and a test pins it, because a rung that quietly stopped firing would be a silent change in
/// what the engine believes.
#[must_use]
pub fn rung_name(evidence: &Evidence) -> &'static str {
    match evidence {
        Evidence::Containment => "containment",
        Evidence::ImportBinding { .. } => "import_binding",
        Evidence::ReceiverType { .. } => "receiver_owner",
        Evidence::PathMatch => "path_match",
        Evidence::QualifiedNameInScope { .. } => "scope_qualified_name",
        Evidence::SameFile => "same_file",
        Evidence::UniqueName => "unique_name",
        Evidence::NameOnly => "name_only",
    }
}

/// What one resolution pass did, measured.
///
/// Every number here is counted. A number that cannot be counted is not reported, because a
/// plausible zero is how the predecessor's `explain()` came to describe an empty caller list as
/// a fact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolutionReport {
    /// Relations the pass looked at.
    pub examined: u64,
    /// Proven: an import binding, an identified receiver, a located module, or a definition in
    /// the same file.
    pub resolved: u64,
    /// A claim rather than a proof, with the claim written into the relation's `basis`.
    pub inferred: u64,
    /// Two or more equally supported candidates. Every one of them was written down.
    pub ambiguous: u64,
    /// No target could be established.
    pub unresolved: u64,
    /// Unresolved relations by reason, so "why" is a number and not a guess.
    pub unresolved_by_reason: BTreeMap<String, u64>,
    /// Relations that were already decided and were decided again, because the entity they
    /// pointed at is inside the pass's scope.
    pub reconsidered: u64,
    /// Edges the caller read *before* its write, whose targets that write removed.
    ///
    /// Reported separately from `reconsidered` because they are a different population: these
    /// arrived from outside the pass's paths and had already been demoted to a null target, so
    /// nothing but the caller's own capture could have found them. A caller repairing a rename
    /// needs this number without inferring it from `examined`.
    pub displaced: u64,
    /// Relation rows the pass actually rewrote.
    pub relations_written: u64,
    /// Candidate lookups abandoned at a configured limit. Non-zero means an answer on this build
    /// may be less complete than the index could have supported.
    pub truncated: u64,
    /// Whether any `Pending` relation is still in the index after the pass.
    pub pending_remaining: bool,
    /// Whether the pass committed. A pass that decided nothing does not.
    pub committed: bool,
    /// The store generation after the pass.
    pub generation: u64,
}

impl ResolutionReport {
    /// The name of this pass, for a report that has to say which one ran.
    pub const PASS: &'static str = "resolution pass 2";

    /// Record one decision.
    fn record(&mut self, decision: &Decision) {
        match decision {
            Decision::Resolved { .. } => self.resolved += 1,
            Decision::Inferred { .. } => self.inferred += 1,
            Decision::Ambiguous { .. } => self.ambiguous += 1,
            Decision::Unresolved { reason } => {
                self.unresolved += 1;
                *self
                    .unresolved_by_reason
                    .entry(reason.as_str().to_owned())
                    .or_insert(0) += 1;
            }
        }
    }

    /// A one-line summary for `peek status` and the MCP `index_status` primitive.
    pub fn summary(&self) -> String {
        let mut reasons: Vec<String> = self
            .unresolved_by_reason
            .iter()
            .map(|(reason, count)| format!("{reason} {count}"))
            .collect();
        reasons.sort();
        let reason_text = match reasons.is_empty() {
            true => String::new(),
            false => format!(" ({})", reasons.join(", ")),
        };
        format!(
            "{}: examined {}, resolved {}, inferred {}, ambiguous {}, unresolved {}{}, \
             reconsidered {}, displaced {}, {} rows at generation {}{}{}",
            Self::PASS,
            self.examined,
            self.resolved,
            self.inferred,
            self.ambiguous,
            self.unresolved,
            reason_text,
            self.reconsidered,
            self.displaced,
            self.relations_written,
            self.generation,
            match self.committed {
                true => "",
                false => ", no commit",
            },
            match self.truncated {
                0 => String::new(),
                other => format!(", {other} lookup(s) truncated at a limit"),
            },
        )
    }
}

/// What the ladder concluded about one relation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// Proven. The target exists and the evidence justifies it.
    Resolved { target: EntityId, by: Evidence },
    /// A claim rather than a proof, with the claim written out.
    Inferred {
        target: EntityId,
        by: Evidence,
        basis: String,
    },
    /// More than one equally supported candidate, strongest evidence first. The first is *not* a
    /// recommendation.
    Ambiguous { candidates: Vec<EntityId> },
    /// No target could be established. Written back with this reason rather than dropped, because
    /// a relation that resolved to nothing has to stay countable.
    Unresolved { reason: UnresolvedReason },
}

/// A candidate target and the evidence that put it on the list.
#[derive(Debug, Clone)]
struct Found {
    id: EntityId,
    by: Evidence,
    /// The candidate was only reached through a guess — a case-folded comparison. A guess is
    /// never allowed to become a `Resolved`.
    guessed: bool,
}

/// A local name bound by an import declaration, as the extractor recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportBinding {
    local: String,
    module: String,
    alias: Option<String>,
}

/// The module and alias of an import relation, whichever decided state it is in.
///
/// An import relation is `Pending` until this pass runs, so that is the common case; one already
/// decided by an earlier pass is re-read when its module moves, and its evidence has to come from
/// wherever it now lives.
fn import_evidence(state: &ResolutionState) -> Option<(String, Option<String>)> {
    match state {
        ResolutionState::Pending {
            evidence: Evidence::ImportBinding { module, alias },
            ..
        }
        | ResolutionState::Resolved {
            by: Evidence::ImportBinding { module, alias },
        }
        | ResolutionState::Inferred {
            by: Evidence::ImportBinding { module, alias },
            ..
        } => Some((module.clone(), alias.clone())),
        _ => None,
    }
}

/// The resolver's per-pass state: the options, the read cache, and the truncation counter.
struct Resolver<'s> {
    store: &'s Store,
    options: ResolutionOptions,
    /// The import bindings of each file this pass has already read. A file is read once.
    imports: BTreeMap<RepoPath, Vec<ImportBinding>>,
    /// Lookups abandoned at a limit, carried into the report rather than hidden.
    truncated: u64,
}

impl<'s> Resolver<'s> {
    fn new(store: &'s Store, options: ResolutionOptions) -> Self {
        Self {
            store,
            options,
            imports: BTreeMap::new(),
            truncated: 0,
        }
    }

    /// Read the entities of one file, noting whether the limit truncated the read.
    fn entities_in_file(
        &mut self,
        path: &RepoPath,
    ) -> Result<Vec<crate::model::Entity>, StoreError> {
        let limit = self.options.entities_per_file;
        let found = self.store.entities_in_file(path, limit)?;
        if found.len() >= limit {
            self.truncated += 1;
        }
        Ok(found)
    }

    /// Read the entities carrying one name, noting whether the limit truncated the read.
    fn entities_named(&mut self, name: &str) -> Result<Vec<crate::model::Entity>, StoreError> {
        let limit = self.options.entities_by_name;
        let found = self.store.entities_named(name, limit)?;
        if found.len() >= limit {
            self.truncated += 1;
        }
        Ok(found)
    }

    // -----------------------------------------------------------------------
    // The ladder
    // -----------------------------------------------------------------------

    /// Decide one relation. The rungs are tried strongest first.
    fn decide(&mut self, relation: &Relation) -> Result<Decision, StoreError> {
        let receiver = match &relation.resolution {
            ResolutionState::Pending {
                evidence: Evidence::ReceiverType { receiver },
                ..
            } => Some(receiver.clone()),
            _ => None,
        };
        let scope = match &relation.resolution {
            ResolutionState::Pending {
                evidence: Evidence::QualifiedNameInScope { scope },
                ..
            } => Some(scope.clone()),
            _ => None,
        };

        // A glob import binds no single name, so no rule can ever prove a target for it. Saying
        // so beats falling through to a name lookup on the character `*`, which is roughly what
        // the predecessor's `looks_like_module` heuristic amounted to.
        if relation.kind == RelationKind::Imports && relation.target_name == GLOB {
            return Ok(Decision::Unresolved {
                reason: UnresolvedReason::Unsupported,
            });
        }

        // R1. Skipped for a receiver: a receiver and an import of one name are two claims about
        // the same symbol, and the receiver is the more specific of the two.
        if receiver.is_none()
            && let Some(decision) = self.via_import_binding(relation)?
        {
            return Ok(decision);
        }

        // R2. Terminal: it decides, or the relation is unresolved.
        if let Some(receiver) = receiver {
            return self.via_receiver(relation, &receiver);
        }

        // R3.
        if let Some(scope) = scope
            && let Some(decision) = self.via_scope(relation, &scope)?
        {
            return Ok(decision);
        }

        // R4.
        if let Some(decision) = self.via_same_file(relation)? {
            return Ok(decision);
        }

        // R5.
        if let Some(decision) = self.via_unique_name(relation)? {
            return Ok(decision);
        }

        Ok(Decision::Unresolved {
            reason: reason_for_nothing_found(&relation.target_name),
        })
    }

    /// R1: the name is bound by an import in the referring file.
    fn via_import_binding(&mut self, relation: &Relation) -> Result<Option<Decision>, StoreError> {
        let source_path = relation.source.path().clone();
        let bindings = self.bindings_for(&relation.target_name, &source_path)?;
        if bindings.is_empty() {
            return Ok(None);
        }

        let mut found: Vec<Found> = Vec::new();
        for binding in &bindings {
            let evidence = Evidence::ImportBinding {
                module: binding.module.clone(),
                alias: binding.alias.clone(),
            };
            for id in self.targets_of(binding, &source_path)? {
                if !found.iter().any(|already| already.id == id) {
                    found.push(Found {
                        id,
                        by: evidence.clone(),
                        guessed: false,
                    });
                }
            }
        }
        Ok(match found.is_empty() {
            true => None,
            false => Some(
                self.decide_candidates(found, "resolved through an import binding in this file"),
            ),
        })
    }

    /// The import bindings of one file whose local name is `name`.
    ///
    /// Owned rather than borrowed: the file's bindings live in this resolver's cache, and holding
    /// a borrow of that cache across the `self.targets_of` call below would make the resolver
    /// immutable for the whole lookup.
    fn bindings_for(
        &mut self,
        name: &str,
        path: &RepoPath,
    ) -> Result<Vec<ImportBinding>, StoreError> {
        if !self.imports.contains_key(path) {
            let limit = self.options.outgoing_per_source;
            let mut bindings = Vec::new();
            let entities = self.entities_in_file(path)?;
            for entity in &entities {
                let relations =
                    self.store
                        .outgoing(&entity.id, Some(RelationKind::Imports), limit)?;
                if relations.len() >= limit {
                    self.truncated += 1;
                }
                for relation in relations {
                    if let Some((module, alias)) = import_evidence(&relation.resolution) {
                        bindings.push(ImportBinding {
                            local: relation.target_name.clone(),
                            module,
                            alias,
                        });
                    }
                }
            }
            self.imports.insert(path.clone(), bindings);
        }
        let all = self
            .imports
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(all
            .iter()
            .filter(|binding| binding.local == name)
            .cloned()
            .collect())
    }

    /// The entities an import binding names.
    ///
    /// An aliased binding (`use a::b as c`) names a module, so the target is that module's file
    /// and the whole path is the module. An unaliased binding names an item inside a module, so
    /// the target is an entity carrying that name in the module's file, and both readings of the
    /// path are tried — because `use a::b::c` may mean the item `c` in module `a::b` or the
    /// module `a::b::c`, and nothing in the syntax says which. If both exist the caller gets an
    /// `Ambiguous`, which is the honest answer; reporting one would be a pick.
    fn targets_of(
        &mut self,
        binding: &ImportBinding,
        importer: &RepoPath,
    ) -> Result<Vec<EntityId>, StoreError> {
        if binding.local == GLOB {
            return Ok(Vec::new());
        }
        let aliased = binding.alias.is_some();
        let mut found: Vec<EntityId> = Vec::new();
        for file in files_for_module(&binding.module, importer, !aliased) {
            for entity in self.entities_in_file(&file)? {
                let wanted = match &binding.alias {
                    Some(_) => entity.kind() == EntityKind::File,
                    None => is_declaration(entity.kind()) && entity.name == binding.local,
                };
                if wanted {
                    found.push(entity.id.clone());
                }
            }
        }
        Ok(found)
    }

    /// R2: the receiver names an owner, and the target is declared inside that owner.
    fn via_receiver(
        &mut self,
        relation: &Relation,
        receiver: &str,
    ) -> Result<Decision, StoreError> {
        let name = relation.target_name.as_str();
        let in_file = self.entities_in_file(relation.source.path())?;

        // `self`, `this` and `Self` name the enclosing declaration, and the source's qualified
        // name already carries it exactly. No guess is involved.
        let owners: Vec<(String, bool)> = if matches!(receiver, "self" | "this" | "Self") {
            match enclosing_owner(relation) {
                Some(owner) => vec![(owner, false)],
                None => Vec::new(),
            }
        } else {
            let exact: Vec<String> = in_file
                .iter()
                .filter(|entity| entity.name == receiver)
                .map(|entity| entity.name.clone())
                .collect();
            if !exact.is_empty() {
                exact.into_iter().map(|owner| (owner, false)).collect()
            } else {
                // The documented case-fold, and the only one in this file. A local variable
                // called `service` and the type `Service` are the same thing in every language
                // here, but the match is a guess, so it is recorded as one: the decision becomes
                // `Inferred` and its basis says that letter case was ignored.
                in_file
                    .iter()
                    .filter(|entity| entity.name.eq_ignore_ascii_case(receiver))
                    .map(|entity| (entity.name.clone(), true))
                    .collect()
            }
        };

        if owners.is_empty() {
            return Ok(Decision::Unresolved {
                reason: UnresolvedReason::NoCandidate,
            });
        }

        let mut found: Vec<Found> = Vec::new();
        for (owner, guessed) in &owners {
            let prefix = format!("{owner}.");
            for entity in &in_file {
                if entity.name == name && entity.id.qualified_name().starts_with(&prefix) {
                    found.push(Found {
                        id: entity.id.clone(),
                        by: Evidence::ReceiverType {
                            receiver: receiver.to_owned(),
                        },
                        guessed: *guessed,
                    });
                }
            }
        }
        Ok(match found.is_empty() {
            // Terminal on purpose. Falling through here would bind `service.retry()` to an
            // unrelated free function named `retry`, which is audit B3 exactly.
            true => Decision::Unresolved {
                reason: UnresolvedReason::NoCandidate,
            },
            false => self.decide_candidates(
                found,
                &format!(
                    "the receiver `{receiver}` was matched to a type declared in this file while \
                     ignoring letter case, which is an inference and not a proof"
                ),
            ),
        })
    }

    /// R3: a multi-segment path naming a module this file can locate.
    fn via_scope(
        &mut self,
        relation: &Relation,
        scope: &str,
    ) -> Result<Option<Decision>, StoreError> {
        let name = relation.target_name.as_str();
        let evidence = Evidence::QualifiedNameInScope {
            scope: scope.to_owned(),
        };
        let mut found: Vec<Found> = Vec::new();
        for file in files_for_module(scope, relation.source.path(), true) {
            for entity in self.entities_in_file(&file)? {
                if entity.name == name && is_declaration(entity.kind()) {
                    found.push(Found {
                        id: entity.id.clone(),
                        by: evidence.clone(),
                        guessed: false,
                    });
                }
            }
        }
        Ok(match found.is_empty() {
            true => None,
            false => Some(self.decide_candidates(
                found,
                "resolved through a qualified path this file can locate",
            )),
        })
    }

    /// R4: the target is declared in the same file as the reference.
    fn via_same_file(&mut self, relation: &Relation) -> Result<Option<Decision>, StoreError> {
        let name = relation.target_name.as_str();
        let mut found: Vec<Found> = Vec::new();
        for entity in self.entities_in_file(relation.source.path())? {
            if entity.name == name && is_declaration(entity.kind()) {
                found.push(Found {
                    id: entity.id.clone(),
                    by: Evidence::SameFile,
                    guessed: false,
                });
            }
        }
        Ok(match found.is_empty() {
            true => None,
            false => Some(self.decide_candidates(found, "resolved to a definition in this file")),
        })
    }

    /// R5: exactly one entity in the repository carries the name.
    ///
    /// The only rung that leaves the source file, and the only one whose answer is never a
    /// proof: uniqueness is a fact about the index, not a statement about intent. Two or more
    /// matches is an `Ambiguous` — the case the predecessor answered with the alphabetically
    /// first file in the whole repository.
    fn via_unique_name(&mut self, relation: &Relation) -> Result<Option<Decision>, StoreError> {
        let name = relation.target_name.as_str();
        let mut found: Vec<Found> = Vec::new();
        for entity in self.entities_named(name)? {
            if is_declaration(entity.kind()) {
                found.push(Found {
                    id: entity.id.clone(),
                    by: Evidence::UniqueName,
                    guessed: false,
                });
            }
        }
        if found.is_empty() {
            return Ok(None);
        }
        // No sort needed: `Store::entities_named` orders by `path, kind, qualified_name,
        // entity_ordinal`, which is exactly `EntityId`'s own ordering, so the candidates arrive
        // in the order they will be written in.
        if found.len() > 1 {
            return Ok(Some(Decision::Ambiguous {
                candidates: self.cap(found.into_iter().map(|f| f.id).collect()),
            }));
        }
        let only = found.remove(0);
        // The basis names where the single candidate is, so `peek explain` can be checked against
        // the file rather than taken on trust. Built before the move, for the obvious reason.
        let basis = format!(
            "exactly one entity named `{name}` is indexed, at {}; a name that happens to be \
             unique in this repository is a claim about the index, not about the code",
            only.id
        );
        Ok(Some(Decision::Inferred {
            target: only.id,
            by: Evidence::UniqueName,
            basis,
        }))
    }

    /// Turn one rung's candidate set into a decision.
    ///
    /// Candidates are ordered by evidence strength and then by identity, so the stored list is
    /// deterministic and reproducible across runs. Within one rung the strength is equal by
    /// construction, so in practice the identity tiebreak is what decides the order — and that is
    /// exactly why the order is documented as a *presentation* order. Nothing in this file ever
    /// takes the first candidate, and the first candidate in a stored `Ambiguous` is not a
    /// recommendation; it is only the one that sorts first.
    ///
    /// `guess_note` is the basis written when the candidate was only reached through a
    /// case-folded match. A `Resolved` decision writes no basis, because `ResolutionState` has no
    /// basis field for it; the rule is readable from the evidence class alone, which is what
    /// [`rule_name`] returns.
    fn decide_candidates(&mut self, mut found: Vec<Found>, guess_note: &str) -> Decision {
        // Sorted by hand rather than with `sort_by_key`, because the key is a tuple containing a
        // `Reverse` and a `String`-bearing identity, and the point of the comparison is to be
        // readable: strongest evidence first, then a stable identity order.
        found.sort_by(|a, b| {
            b.by.strength()
                .cmp(&a.by.strength())
                .then_with(|| a.id.cmp(&b.id))
        });

        // Drop duplicate identities before counting, keeping the first occurrence — which, after
        // the sort, is the one with the strongest evidence. Without this, a case-folded match
        // reports the *same* entity twice and calls it ambiguity: the edge comes back
        // `Ambiguous { [X, X] }`, which is not uncertainty but a bug that reads exactly like it.
        // An agent shown two identical candidates cannot tell a real ambiguity from a
        // double-count, so the two are indistinguishable at the point where it matters most.
        found.dedup_by(|a, b| a.id == b.id);
        if found.len() > 1 {
            return Decision::Ambiguous {
                candidates: self.cap(found.into_iter().map(|f| f.id).collect()),
            };
        }
        // `dedup_by` can empty the list, so this is not a "there is one" assumption.
        let Some(mut only) = found.pop() else {
            return Decision::Unresolved {
                reason: UnresolvedReason::NoCandidate,
            };
        };
        match only.guessed {
            false => Decision::Resolved {
                target: only.id,
                by: only.by,
            },
            true => Decision::Inferred {
                target: only.id,
                by: only.by,
                basis: guess_note.to_owned(),
            },
        }
    }

    /// Apply the candidate cap, recording that it was applied.
    fn cap(&mut self, mut candidates: Vec<EntityId>) -> Vec<EntityId> {
        if candidates.len() > self.options.max_candidates {
            self.truncated += 1;
            candidates.truncate(self.options.max_candidates);
        }
        candidates
    }
}

/// Whether the ladder should decide this relation at all.
///
/// Structural relations are excluded, and the reason is that re-deciding one can only lose
/// information. `Defines`, `Contains` and `Owns` are settled by grammar — the enclosing node *is*
/// the target — and the walker already wrote them as `Resolved { by: Containment }`. That is not
/// "pending, awaiting a decision"; it is a decision, and the strongest one the model has. Running
/// the ladder over one would replace `containment` with `same_file`, which is a downgrade the
/// report would then present as an improvement.
fn is_resolvable(relation: &Relation) -> bool {
    !relation.kind.is_structural()
}

/// Whether an entity kind can be the target of a name reference.
///
/// `File` is excluded because audit B21 is exactly what happens when it is not: a file whose
/// name matches a symbol wins a name lookup and produces a well-formed empty answer instead of a
/// reported ambiguity. A module is only ever a target through R1, which reaches one deliberately.
fn is_declaration(kind: EntityKind) -> bool {
    kind != EntityKind::File
}

/// The declaration enclosing `relation.source`, if it has one.
///
/// `A.b.c` is declared inside `A.b`, so the owner of `c` is the whole prefix. A file entity has
/// no enclosing declaration, and neither does a top-level function.
fn enclosing_owner(relation: &Relation) -> Option<String> {
    if !relation.source.kind().is_member() {
        return None;
    }
    let (head, _) = relation.source.qualified_name().rsplit_once('.')?;
    match head.is_empty() {
        true => None,
        false => Some(head.to_owned()),
    }
}

/// Why a name could not be bound, given that no rung found anything for it.
///
/// A bare name matching nothing in the repository is one fact. A *qualified* name matching
/// nothing — `std::fmt::Debug`, `java.util.List`, `../payments/service` — is a different fact:
/// the thing named lives in a standard library, a third-party dependency, or a file the indexer
/// refused. Reporting both as `NoCandidate` would make the unresolved bucket a number nobody can
/// act on, and the distinction is the one `UnresolvedReason::External` exists for.
fn reason_for_nothing_found(target_name: &str) -> UnresolvedReason {
    let qualified = target_name.contains("::")
        || target_name.contains('/')
        || (target_name.contains('.') && !target_name.starts_with('.'));
    match qualified {
        true => UnresolvedReason::External,
        false => UnresolvedReason::NoCandidate,
    }
}

/// The files a module path could name, as seen from `importer`.
///
/// Three details of this are compromises and are stated rather than hidden:
///
/// * **There is no module table.** Peek has `EntityKind::Module` and no way to populate it, so a
///   module path is turned into file paths and each is looked up through the primary key. This
///   function is the reason that gap matters, and it is also the argument for closing it: a
///   `Module` entity per module would replace a handful of seeks with one, and it belongs in the
///   extractor rather than here — a resolver that invented module rows to speed up its own
///   lookups would be a second source of truth (contract H5).
/// * **The anchor list is the referring file's directory and its ancestors.** `crate::` and a
///   bare path use the same list, because the crate root is not recorded anywhere; `super::`
///   skips one anchor per occurrence. The list stops at [`MAX_ANCHOR_DEPTH`] so a deep
///   repository cannot make one import cost an unbounded number of seeks.
/// * **Two readings.** With `strip_last`, the whole path may be a module, or the last segment
///   may be an item inside the module named by the rest. Both are real, both are tried, and if
///   both files exist the caller sees an `Ambiguous` rather than a pick.
///
/// Directory-style module files (`mod.rs`) are recognised only for Rust, where the resolver can
/// see the importer's extension. Every other language's layout is a follow-up: it needs a
/// per-language module convention, which is data and belongs in a `LanguageSpec` rather than in
/// a path test here.
fn files_for_module(module: &str, importer: &RepoPath, strip_last: bool) -> Vec<RepoPath> {
    let mut segments: Vec<String> = Vec::new();
    let mut climb = 0usize;
    for part in module.split("::") {
        match part {
            "" | "crate" | "self" => continue,
            "super" => climb += 1,
            other => segments.push(other.to_owned()),
        }
    }
    if segments.is_empty() || segments.len() > MAX_MODULE_SEGMENTS {
        return Vec::new();
    }

    let extension = importer.extension();
    let mut readings: Vec<Vec<String>> = vec![segments.clone()];
    if strip_last && segments.len() > 1 {
        readings.push(segments[..segments.len() - 1].to_vec());
    }

    let anchors = anchor_directories(importer);
    let mut files: Vec<RepoPath> = Vec::new();
    for reading in &readings {
        let joined = reading.join("/");
        for (depth, anchor) in anchors.iter().enumerate() {
            if depth < climb {
                continue;
            }
            let stem = match anchor.is_empty() {
                true => joined.clone(),
                false => format!("{anchor}/{joined}"),
            };
            if let Some(extension) = &extension
                && let Some(path) = RepoPath::new(format!("{stem}.{extension}"))
            {
                push_new(&mut files, path);
            }
            if extension.as_deref() == Some("rs")
                && let Some(path) = RepoPath::new(format!("{stem}/mod.rs"))
            {
                push_new(&mut files, path);
            }
        }
    }
    files
}

/// Add a candidate file if it has not been produced already.
fn push_new(files: &mut Vec<RepoPath>, path: RepoPath) {
    if !files.contains(&path) {
        files.push(path);
    }
}

/// The directories a module path is anchored at: the importer's own directory, then each
/// directory above it, up to the repository root.
fn anchor_directories(importer: &RepoPath) -> Vec<String> {
    let mut anchors = Vec::new();
    let mut current = importer
        .parent()
        .map_or_else(String::new, |parent| parent.as_str().to_owned());
    for _ in 0..=MAX_ANCHOR_DEPTH {
        anchors.push(current.clone());
        if current.is_empty() {
            break;
        }
        current = match current.rsplit_once('/') {
            Some((head, _)) => head.to_owned(),
            None => String::new(),
        };
    }
    anchors
}

/// A de-duplicating set of relations, in the order the store returned them.
///
/// The same relation is reachable as an outgoing edge of a changed file, as an incoming edge of a
/// changed entity, and as an edge a refresh displaced, so a pass over a scope collects the union
/// and has to de-duplicate it. Deciding the same relation twice would write it twice and count it
/// twice in the report.
///
/// De-duplication is by [`Relation::natural_key`], which is the store's own `UNIQUE` constraint
/// rather than a second opinion about what identity means.
#[derive(Debug, Default)]
struct RelationSet {
    seen: BTreeSet<RelationKey>,
    relations: Vec<Relation>,
}

impl RelationSet {
    fn new() -> Self {
        Self::default()
    }

    /// Add a relation unless this pass has already collected the same one.
    fn insert(&mut self, relation: Relation) {
        if self.seen.insert(relation.natural_key()) {
            self.relations.push(relation);
        }
    }

    fn into_vec(self) -> Vec<Relation> {
        self.relations
    }
}

/// Write one decision back over the relation the store already holds.
///
/// Returns `None` when the decision is identical to what is already stored, which is what makes
/// a second pass over an unchanged index a genuine no-op: no row is rewritten, the batch stays
/// empty, no commit happens, and the generation does not move.
fn apply_decision(relation: &Relation, decision: Decision) -> Option<Relation> {
    let (target, resolution) = match decision {
        Decision::Resolved { target, by } => (Some(target), ResolutionState::Resolved { by }),
        Decision::Inferred { target, by, basis } => {
            (Some(target), ResolutionState::Inferred { by, basis })
        }
        Decision::Ambiguous { candidates } => (None, ResolutionState::Ambiguous { candidates }),
        Decision::Unresolved { reason } => (None, ResolutionState::Unresolved { reason }),
    };
    if target.as_ref() == relation.target.as_ref() && resolution == relation.resolution {
        return None;
    }
    Some(Relation {
        kind: relation.kind,
        source: relation.source.clone(),
        target_name: relation.target_name.clone(),
        target,
        span: relation.span,
        resolution,
    })
}

/// Decide every `Pending` relation in the index, in one pass and one commit.
///
/// This is the second half of a full build: the extractor has committed, and this turns what it
/// wrote into decisions. The whole pending set is read in one query because the store offers no
/// cursor to page it — and because the indexer already materialises the entire relation set in
/// memory in order to write it, so this is the same order of footprint rather than a new one.
///
/// `reconsider_decided` is **ignored** here, and that is deliberate rather than an oversight. A
/// full build has just re-extracted every file, so every relation in the index was re-emitted as
/// `Pending` and nothing is left to reconsider; a refresh is the case that needs it, and that is
/// [`resolve_paths`]. Re-deciding decided edges on a full build would double the work for no
/// gain and would re-write every `Contains` edge the extractor had already settled.
pub fn resolve_all(
    store: &mut Store,
    options: ResolutionOptions,
) -> Result<ResolutionReport, StoreError> {
    let mut in_scope = RelationSet::new();
    for relation in store.relations_in_state(&pending_state(), usize::MAX)? {
        if is_resolvable(&relation) {
            in_scope.insert(relation);
        }
    }
    decide_and_commit(store, in_scope.into_vec(), options)
}

/// Decide the relations belonging to `paths`, the relations that point into them, and the
/// relations the caller displaced.
///
/// Scoped, not global, because a refresh knows which files changed and nothing else has.
///
/// `displaced` is the awkward part and it is not optional. A refresh *removes* the changed
/// files' rows before re-inserting them, and the store's demotion step turns every edge that
/// pointed into a removed entity into an `Unresolved` with a null target. By the time the
/// resolver runs, those edges are invisible to [`Store::incoming`], which matches on
/// `target_path`. So the caller has to read them **before** the write and hand them over here,
/// or a moved definition silently orphans every one of its callers.
///
/// The ordering — read the edges that are about to be broken, write, then re-decide — is the
/// whole of contract G9, and it is why this function takes three arguments rather than one.
pub fn resolve_paths(
    store: &mut Store,
    paths: &[RepoPath],
    displaced: &[Relation],
    options: ResolutionOptions,
) -> Result<ResolutionReport, StoreError> {
    let mut in_scope = RelationSet::new();
    let mut displaced_keys: BTreeSet<RelationKey> = BTreeSet::new();
    for relation in displaced {
        if is_resolvable(relation) {
            in_scope.insert(relation.clone());
            displaced_keys.insert(relation.natural_key());
        }
    }

    for path in paths {
        // A file is read through two doors: its own outgoing edges, and the edges arriving at the
        // entities it declares. The second is the "a definition moved" half.
        let entities = store.entities_in_file(path, options.entities_per_file)?;
        for entity in &entities {
            let outgoing_limit = options.outgoing_per_source;
            for relation in store.outgoing(&entity.id, None, outgoing_limit)? {
                if relation.resolution.is_pending() {
                    in_scope.insert(relation);
                }
            }
            if options.reconsider_decided {
                let incoming_limit = options.incoming_per_entity;
                for relation in store.incoming(&entity.id, None, incoming_limit)? {
                    if is_resolvable(&relation) {
                        in_scope.insert(relation);
                    }
                }
            }
        }
    }

    let mut report = decide_and_commit(store, in_scope.into_vec(), options)?;
    // The displaced edges are counted separately because they are a different population: they
    // are the ones a refresh broke, and a caller repairing a rename needs to know how many it
    // repaired without inferring it from `examined`.
    report.displaced = u64::try_from(displaced_keys.len()).unwrap_or(u64::MAX);
    Ok(report)
}

/// The state used to ask the store for everything still awaiting a decision.
///
/// Only the tag is read by the query, so the payload is a placeholder. Building it as a real
/// `Pending` rather than a bare string means this call site cannot drift from the model's own
/// spelling of the state.
fn pending_state() -> ResolutionState {
    ResolutionState::Pending {
        evidence: Evidence::NameOnly,
        basis: String::new(),
    }
}

/// Decide `relations` and write every decision in one transaction.
fn decide_and_commit(
    store: &mut Store,
    relations: Vec<Relation>,
    options: ResolutionOptions,
) -> Result<ResolutionReport, StoreError> {
    let mut report = ResolutionReport {
        examined: relations.len() as u64,
        generation: store.generation(),
        ..ResolutionReport::default()
    };
    let mut update = IndexUpdate::empty();

    {
        let mut resolver = Resolver::new(store, options);
        for relation in &relations {
            if !relation.resolution.is_pending() {
                report.reconsidered += 1;
            }
            let decision = resolver.decide(relation)?;
            report.record(&decision);
            if let Some(decided) = apply_decision(relation, decision) {
                update = update.with_relation(decided);
            }
        }
        // Read out of the scope that owns the resolver, so the counter cannot be forgotten at
        // the point of construction and silently report zero.
        report.truncated = resolver.truncated;
    }

    if !update.is_empty() {
        let stats = store.apply_update(update)?;
        report.committed = true;
        report.relations_written = stats.relations_upserted;
        report.generation = stats.generation;
    }
    // Measured, not assumed. The pass decided everything it looked at, so the only question
    // left is whether anything was still pending when it looked.
    report.pending_remaining = !store.relations_in_state(&pending_state(), 1)?.is_empty();
    Ok(report)
}
