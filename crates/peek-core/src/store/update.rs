//! The write batch.
//!
//! # Why a batch and not a single row
//!
//! An index update is never one fact. Re-indexing a file produces its entities, the relations
//! that originate in it, the relations that *point into* it, and the removal of everything that
//! used to be there. Committing those independently means a reader can observe a file that is
//! half indexed — a real state in which the file's `Calls` edges exist but its callee does not,
//! which produces a confidently wrong answer rather than an error.
//!
//! [`IndexUpdate`] is therefore the unit of atomicity: [`Store::apply_update`] either lands all
//! of it or none of it, and the generation only advances on the success path.

use crate::model::entity::Entity;
use crate::model::path::RepoPath;
use crate::model::relation::Relation;

/// What a batch of writes should do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexUpdate {
    /// Entities to insert or replace, keyed by their own identity.
    ///
    /// Order is irrelevant: a second upsert of the same identity replaces the first.
    pub upserted_entities: Vec<Entity>,
    /// Relations to insert or replace, keyed by their natural key
    /// (source, kind, target name, span).
    ///
    /// A relation's source entity must already exist, or must appear in
    /// [`Self::upserted_entities`]. Removals are applied first, so an entity removed and
    /// re-added in the same batch ends up present.
    pub upserted_relations: Vec<Relation>,
    /// Paths whose contents are no longer in the index.
    pub removed_paths: Vec<Removal>,
}

impl IndexUpdate {
    /// An update that changes nothing. Still a valid, committing update: it bumps the
    /// generation, because a commit happened and a reader comparing generations must see it.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether this update would write or delete anything at all.
    pub fn is_empty(&self) -> bool {
        self.upserted_entities.is_empty()
            && self.upserted_relations.is_empty()
            && self.removed_paths.is_empty()
    }

    /// Add one entity. Chaining, so a caller building a batch from an iterator does not have to
    /// name the field at every step.
    #[must_use]
    pub fn with_entity(mut self, entity: Entity) -> Self {
        self.upserted_entities.push(entity);
        self
    }

    /// Add one relation.
    #[must_use]
    pub fn with_relation(mut self, relation: Relation) -> Self {
        self.upserted_relations.push(relation);
        self
    }

    /// Remove every entity declared in one file, together with every relation that originates
    /// in it and every relation that points into it.
    #[must_use]
    pub fn removing_file(mut self, path: RepoPath) -> Self {
        self.removed_paths.push(Removal::RemoveFile(path));
        self
    }

    /// Remove every entity at or below `path`, together with the relations touching them.
    ///
    /// Used when a directory disappears: deleting each file individually turns one filesystem
    /// event into hundreds of transactions, and a crash midway leaves a partly-deleted tree.
    #[must_use]
    pub fn removing_subtree(mut self, path: RepoPath) -> Self {
        self.removed_paths.push(Removal::RemoveSubtree(path));
        self
    }
}

/// What to forget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    /// Everything declared in exactly this file.
    RemoveFile(RepoPath),
    /// Everything at or below this path, including the path itself.
    RemoveSubtree(RepoPath),
}

impl Removal {
    /// The path the removal is anchored at.
    pub fn path(&self) -> &RepoPath {
        match self {
            Removal::RemoveFile(path) | Removal::RemoveSubtree(path) => path,
        }
    }

    /// Whether this removal also takes the contents of nested directories.
    pub fn includes_subdirectories(&self) -> bool {
        matches!(self, Removal::RemoveSubtree(_))
    }
}

/// What a successful commit did.
///
/// Reported only on the success path. A failure returns `Err` and no statistics at all: a
/// plausible-looking `UpdateStats` from a transaction that rolled back is precisely the
/// dishonesty audit A1 documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateStats {
    /// Entity rows written or replaced.
    pub entities_upserted: u64,
    /// Entity rows deleted.
    pub entities_removed: u64,
    /// Relation rows written or replaced.
    pub relations_upserted: u64,
    /// Relation rows deleted, whether by cascade or directly.
    pub relations_removed: u64,
    /// Candidate rows written, which is the number of distinct candidates across all ambiguous
    /// relations in the batch.
    pub candidates_upserted: u64,
    /// The generation *after* this commit. The number before it is `generation - 1`.
    pub generation: u64,
}

#[cfg(test)]
mod tests {
    use super::{IndexUpdate, Removal, UpdateStats};
    use crate::model::entity::{Entity, EntityId, EntityKind};
    use crate::model::path::RepoPath;
    use crate::model::span::Span;

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).expect("valid path")
    }

    fn entity(file: &str, name: &str) -> Entity {
        Entity {
            id: EntityId::new(path(file), EntityKind::Function, name, 0),
            name: name.to_owned(),
            signature: None,
            doc: None,
            span: Span::new(0, 10, 1, 1, 1, 11),
            language: Some(crate::model::language::Language::Rust),
            is_test: false,
            structural_fingerprint: None,
        }
    }

    #[test]
    fn an_empty_update_is_recognised_as_empty() {
        let update = IndexUpdate::empty();
        assert!(update.is_empty());
        // The builders consume `self` and return the new value, so each assertion needs its own
        // starting point rather than reusing a value that has already been moved.
        assert!(!update.clone().with_entity(entity("src/a.rs", "main")).is_empty());
        assert!(!update.removing_file(path("src/a.rs")).is_empty());
    }

    #[test]
    fn the_builder_accumulates_every_kind_of_change() {
        let update = IndexUpdate::empty()
            .with_entity(entity("src/a.rs", "main"))
            .with_entity(entity("src/b.rs", "helper"))
            .removing_file(path("src/gone.rs"))
            .removing_subtree(path("src/old"));
        assert_eq!(update.upserted_entities.len(), 2);
        assert_eq!(
            update.removed_paths,
            vec![
                Removal::RemoveFile(path("src/gone.rs")),
                Removal::RemoveSubtree(path("src/old")),
            ]
        );
    }

    #[test]
    fn the_two_removal_variants_report_different_scope() {
        let file = Removal::RemoveFile(path("src/a.rs"));
        let subtree = Removal::RemoveSubtree(path("src"));
        assert!(!file.includes_subdirectories());
        assert!(subtree.includes_subdirectories());
        assert_eq!(file.path(), &path("src/a.rs"));
        assert_eq!(subtree.path(), &path("src"));
    }

    #[test]
    fn the_default_update_matches_an_empty_one() {
        assert_eq!(IndexUpdate::default(), IndexUpdate::empty());
        assert!(IndexUpdate::default().is_empty());
    }

    #[test]
    fn statistics_are_plain_data_with_no_derived_fields() {
        // Nothing here computes anything: every number is counted at the point of the write.
        let stats = UpdateStats {
            entities_upserted: 3,
            entities_removed: 1,
            relations_upserted: 4,
            relations_removed: 2,
            candidates_upserted: 6,
            generation: 9,
        };
        let same = UpdateStats {
            entities_upserted: 3,
            entities_removed: 1,
            relations_upserted: 4,
            relations_removed: 2,
            candidates_upserted: 6,
            generation: 9,
        };
        assert_eq!(stats, same);
    }
}
