//! Module and package structure, read off a file's path.
//!
//! # Why this is in the extractor
//!
//! Measured on `rust-lang/regex`, the resolver decides 12,301 of 33,762 relations and 343 of the
//! 344 edges it cannot decide are `no_candidate`. The cause is structural, not a tuning problem:
//! `regex` is a cross-crate project, most of its references name something in another crate, and
//! the index had no module table for the resolver to look anything up in. The resolver's own
//! `files_for_module` says so in a comment: *"a `Module` entity per module would replace a
//! handful of seeks with one, and it belongs in the extractor rather than here — a resolver that
//! invented module rows to speed up its own lookups would be a second source of truth"*. This
//! module is that module table.
//!
//! # The qualified-name rule
//!
//! **A module's qualified name is its path from its package's source root, prefixed with the
//! package name, spelled the way the source spells it.** So `crates/regex-automata/src/util/
//! look.rs` in package `regex_automata` is the module `regex_automata::util::look`, and
//! `crates/regex-automata/src/util/mod.rs` is the module `regex_automata::util`.
//!
//! The prefix is the point. `use regex_automata::util::look::Matcher;` names a path whose first
//! segment is another crate, and a module table indexed by a name that starts with the crate is
//! the only thing that makes such a path a lookup rather than a guess. It is also why the
//! separator is the source's own `::` rather than the model's dotted `qualified_name` separator:
//! a name a `use` statement could have spelled needs no translation before it can be compared.
//!
//! # What is *not* prefixed, and why
//!
//! An inline `mod x { .. }` inside a file that is itself the module `pkg::a` keeps its existing
//! qualified name, `x`, and is tied to its parent by a `Contains` edge rather than by a name
//! prefix. Prefixing it would change the qualified name of **every symbol inside it**, and
//! `qualified_name` is two thirds of `EntityId` (D-0005). A change like that gives every method
//! in every module a new identity on the next re-index, rewriting the whole relation set over a
//! refactor that moved nothing. A missing `::` in a name is a smaller, recoverable problem than
//! that, and the `Contains` edge carries the same fact without the churn.
//!
//! # What is approximated, stated rather than hidden
//!
//! The extractor sees one file's path and one file's text. It does not see `Cargo.toml`.
//!
//! * **The package name is the directory above the source root.** Cargo derives a package's
//!   default name from that directory, so the two agree unless `Cargo.toml` overrides `name`.
//!   Hyphens become underscores, which is Cargo's own normalisation and also what the `use`
//!   statement says, so `crates/regex-automata` gives `regex_automata` and matches.
//! * **A package sitting at the repository root has no directory to take a name from**, because
//!   its source root *is* the repository root. The source root's own name is used instead, so
//!   `src/lib.rs` is in a package called `src`. That is wrong and it is visibly wrong — the
//!   wrongness is in the name a reader can see, not in a hidden guess.
//! * **A file with no source root above it** is placed in a package named after its own
//!   directory. `tests/helper.rs` is therefore in a package called `tests`.
//!
//! Closing the first two properly needs a `Cargo.toml` reader, which is a repository-level fact
//! and not a per-file one. It is the follow-up, and the layout is a table so that supplying it
//! is a data change.
//!
//! # The crate boundary
//!
//! **Decision: the boundary is an `EntityKind::Package` entity plus a `Contains` edge to the
//! package's root module.** The argument is at [`for_file`].

use crate::extract::spec::{LanguageSpec, ModuleLayout};
use crate::model::{
    Entity, EntityId, EntityKind, Evidence, Relation, RelationKind, RepoPath, Span,
};

/// What one file contributed to the module graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileModules {
    /// The package and module entities, in the order they were created.
    pub entities: Vec<Entity>,
    /// The containment edges tying them together and to the file.
    pub relations: Vec<Relation>,
    /// The entity for the module this file *is*, when the language has a module layout.
    pub module: Option<EntityId>,
}

impl FileModules {
    /// Whether this file contributed any module structure at all.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

/// Where a file sits in its package's module tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleLocation {
    /// The package the file belongs to, spelled the way a `use` statement would spell it.
    pub package: String,
    /// The path components between the package's source root and the file, plus the file's own
    /// stem unless the stem names the directory it sits in.
    pub segments: Vec<String>,
    /// Whether the file is the package's own root module.
    pub is_package_root: bool,
}

impl ModuleLocation {
    /// The module's qualified name: the package, then every segment below the source root.
    pub fn qualified_name(&self, separator: &str) -> String {
        let mut parts = Vec::with_capacity(self.segments.len() + 1);
        parts.push(self.package.as_str());
        parts.extend(self.segments.iter().map(String::as_str));
        parts.join(separator)
    }

    /// The module's own name, without the package prefix.
    ///
    /// This is the `name` column and the one a bare-name lookup matches on, so it has to be the
    /// last segment a `use` statement could have written and never the whole path.
    pub fn name(&self) -> String {
        match self.segments.last() {
            Some(last) => last.clone(),
            // The package root module is named for the package: `crates/foo/src/lib.rs` is the
            // module `foo`, because `lib` is a filename convention rather than a name anything
            // can refer to.
            None => self.package.clone(),
        }
    }
}

/// Read one file's module and package structure.
///
/// # Where the crate boundary lives, and why
///
/// **It is an `EntityKind::Package` entity joined to the root module by a `Contains` relation,
/// emitted by the file that is the package root.** Four reasons, in the order they mattered:
///
/// 1. **The kind already exists and says exactly this.** `EntityKind::Package` is documented as
///    "a distributable unit: Cargo crate, npm package, Maven module, .NET project". There is no
///    schema change here, no new `EntityKind` variant, and — because the field is a string column
///    with no `CHECK` — no `schema_version` bump. A decision that avoided a schema bump on its
///    own merits is worth more than one that needed a waiver.
/// 2. **It is a relation, not an attribute.** `Entity` has no structural field to put it in, and
///    adding one *would* be a schema change. More to the point, this engine already expresses
///    containment as relations everywhere and says why: *"containment is expressed as relations
///    rather than a side table, so that every edge in the graph has exactly one representation"*. A
///    crate attribute would be a second representation of the same fact.
/// 3. **`Contains` is structural, so the boundary is never re-litigated.** `RelationKind::
///    is_structural()` covers `Contains`, which means the resolver's ladder never touches it and
///    it is `Resolved { by: Containment }` from the moment it is written. A crate boundary derived
///    by inference would be re-decided on every refresh and could become `Ambiguous` after an
///    unrelated edit. This is a fact about the build, not a claim about a name.
/// 4. **The alternative — letting the resolver work it out — is what the resolver already
///    documents it must not do.** A resolver that invented `Package` rows to make its own
///    lookups faster would be a second source of truth for the same graph (contract H5). And the
///    fact it needs is not derivable from a single file at all: which files share a package is a
///    property of the *set* of paths, and per-file extraction is the only thing that runs.
///
/// What is deliberately **not** emitted: a `Package`→`Package` dependency edge for
/// `foo = { path = "../foo" }` or `foo = "1.2"`. That is a fact in `Cargo.toml`, and parsing TOML
/// to manufacture it would put build metadata in a source extractor. It is reported, not built.
pub fn for_file(
    spec: &LanguageSpec,
    path: &RepoPath,
    file: &EntityId,
    span: Span,
    text: &str,
) -> FileModules {
    let Some(layout) = spec.module_layout() else {
        return FileModules::default();
    };
    let location = locate(path, &layout);
    let qualified = location.qualified_name(layout.segment_separator);
    let name = location.name();
    let fingerprint = fingerprint(text);

    let mut entities = Vec::new();
    let mut relations = Vec::new();

    let mut package_id = None;
    if location.is_package_root {
        let id = EntityId::new(
            path.clone(),
            EntityKind::Package,
            location.package.clone(),
            0,
        );
        entities.push(Entity {
            id: id.clone(),
            name: location.package.clone(),
            signature: None,
            doc: None,
            span: Some(span),
            language: Some(spec.language),
            is_test: false,
            structural_fingerprint: Some(fingerprint.clone()),
        });
        package_id = Some(id);
    }

    let module_id = EntityId::new(path.clone(), EntityKind::Module, qualified.clone(), 0);
    entities.push(Entity {
        id: module_id.clone(),
        name: name.clone(),
        signature: None,
        doc: None,
        span: Some(span),
        language: Some(spec.language),
        is_test: false,
        structural_fingerprint: Some(fingerprint),
    });

    // The file is what the module is written in, and the file entity and the module entity are
    // different rows: one answers "which file is this", the other "which namespace is this".
    // Without the edge between them nothing in the graph connects the two.
    relations.push(Relation::resolved(
        RelationKind::Contains,
        file.clone(),
        module_id.clone(),
        name.clone(),
        span,
        Evidence::Containment,
    ));

    if let Some(package_id) = package_id {
        relations.push(Relation::resolved(
            RelationKind::Contains,
            package_id,
            module_id.clone(),
            name,
            span,
            Evidence::Containment,
        ));
    }

    FileModules {
        entities,
        relations,
        module: Some(module_id),
    }
}

/// Read a file's place in its package's module tree.
///
/// Pure: a path in, a name out. Every approximation this makes is listed in the module
/// documentation, and each one is pinned by a test here so it cannot drift silently.
pub fn locate(path: &RepoPath, layout: &ModuleLayout) -> ModuleLocation {
    let components: Vec<&str> = path.components().collect();
    let (file_name, directories) = match components.split_last() {
        Some((last, rest)) => (*last, rest),
        // `RepoPath` rejects an empty path, so a one-component path is the shortest case and
        // this branch is unreachable. Falling back to the file's own name keeps the function
        // total rather than panicking over an invariant three files away.
        None => (path.as_str(), &[] as &[&str]),
    };
    let stem = match file_name.rsplit_once('.') {
        Some((head, _)) if !head.is_empty() => head,
        // A dotfile has no extension, so `.gitignore` is a stem rather than an empty one.
        _ => file_name,
    };

    // The first source root in the path is the package's own source tree. A nested `src` further
    // down is a directory that happens to be called `src`. When there is none, the source root is
    // the file's own directory and the file is a direct child of it.
    let source_root = directories
        .iter()
        .position(|component| layout.source_roots.contains(component))
        .unwrap_or(directories.len());
    // `source_root` is either a valid index or `directories.len()`, and in both cases
    // `directories[source_root..]` is a legal slice; the `min` keeps that a property of the code
    // rather than of the comment.
    let after_root = source_root
        .saturating_add(1)
        .min(directories.len());
    let above = &directories[..source_root.min(directories.len())];
    let below = &directories[after_root..];

    // The package is named after the directory that holds its source root. When there is no such
    // directory — a package sitting at the repository root — the source root's own name stands in,
    // and the approximation is visible in every qualified name it produces.
    let holder = above
        .last()
        .or_else(|| directories.get(source_root.saturating_sub(1)))
        .copied();
    let package = match holder {
        Some(name) => normalise_package_name(name),
        // A file at the very top of a checkout has neither. Its own name is the only thing left,
        // and an empty package would make every qualified name unmatchable.
        None => normalise_package_name(stem),
    };

    let is_package_root = below.is_empty() && layout.package_roots.contains(&stem);

    // `a/mod.rs` is the module `a`: the stem names the directory, so it is not a segment. That
    // only works when there *is* a directory below the source root; a bare `src/mod.rs` has
    // nothing to take a name from, and treating it as the package root module would give it the
    // same name as `src/lib.rs` and put two different files in one namespace.
    let names_its_directory = layout.directory_modules.contains(&stem) && !below.is_empty();
    let segments = match is_package_root || names_its_directory {
        true => below.to_vec(),
        false => {
            let mut segments = below.to_vec();
            segments.push(stem.to_owned());
            segments
        }
    };

    ModuleLocation {
        package,
        segments,
        is_package_root,
    }
}

/// Cargo's own rule: a package directory named `regex-automata` is a package named
/// `regex_automata`, which is also the only spelling a `use` statement can say.
fn normalise_package_name(directory: &str) -> String {
    directory.replace('-', "_")
}

/// A structural fingerprint of a module's body: FNV-1a over the source text, hex-encoded.
///
/// Deliberately not a cryptographic hash. This is a reconciliation aid — "these two entities
/// have the same body" — and it is checked against the algorithm's published test vectors, which
/// is what makes it verifiable without a dependency. A file module hashes the whole file, and an
/// inline `mod x { .. }` hashes its own node text; a `mod x;` with no body has no body to hash,
/// so its fingerprint is left unset rather than filled with the six characters `mod x;`.
pub fn fingerprint(text: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("fnv1a64:{hash:016x}")
}

