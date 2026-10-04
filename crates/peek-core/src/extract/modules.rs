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
//! # What is a package, when the layout is not `src/`
//!
//! **A package is the nearest ancestor directory that holds the package's root module.** Everything
//! else follows from that one sentence, and it is a fact about a *directory*, which is the whole
//! of the difficulty: no single file's path says which of its ancestor directories is the one, and
//! per-file extraction is the only thing that runs.
//!
//! So the answer has two cases, and which case applies is decided by the path rather than guessed.
//!
//! 1. **The path contains a source root.** Cargo's own default puts a crate root at
//!    `<package>/<source root>/<lib|main>`, so the source root marks the boundary and the package
//!    is the directory above it. This is read from the path alone and it is exact, and it is the
//!    case every layout in this table was written for. **The repository's file set is not
//!    consulted, so nothing about a `src/` layout changes.** That is the property the rule is
//!    designed around rather than a side effect of it.
//! 2. **The path contains no source root.** Then the path says nothing, and the nearest ancestor
//!    that *directly* holds a root module is the answer: ripgrep has no `src/`, and
//!    `crates/core/main.rs` is the root module of the package rooted at `crates/core`, so
//!    `crates/core/flags/defs.rs` is the module `core::flags::defs` — one package for thirty files
//!    rather than a package per directory. **Nearest, not outermost**: a nested package is a
//!    separate namespace and merging it into the one above it would make every cross-package path
//!    in a workspace unspellable, which is the only thing the table is for.
//!
//! If no ancestor holds a root module, the file's own directory is used and the approximation is
//! visible in the name. `tests/helper.rs` is therefore in a package called `tests`.
//!
//! # What is approximated, stated rather than hidden
//!
//! * **The package name is the directory above the source root.** Cargo derives a package's
//!   default name from that directory, so the two agree unless `Cargo.toml` overrides `name`.
//!   Hyphens become underscores, which is Cargo's own normalisation and also what the `use`
//!   statement says, so `crates/regex-automata` gives `regex_automata` and matches.
//! * **A package sitting at the repository root has no directory to take a name from**, because
//!   its source root *is* the repository root. The source root's own name is used instead, so
//!   `src/lib.rs` is in a package called `src`. That is wrong and it is visibly wrong — the
//!   wrongness is in the name a reader can see, not in a hidden guess. The rule above cannot fix
//!   it either: **a manifest marker would have to name the repository-root package after the
//!   directory the checkout happens to sit in**, and `EntityId` is two thirds `qualified_name`,
//!   so the same repository indexed from two directories would be two different graphs.
//! * **`Cargo.toml` is not read.** A package whose name differs from the directory holding its
//!   root module — ripgrep's `crates/core`, which is the bin of the root package `ripgrep` — is
//!   named for the directory. A `Cargo.toml` reader is the follow-up, and it is a
//!   repository-level fact like the one this module already takes.
//!
//! # A package and a module are two facts, and they are not the same column
//!
//! The `package` on a [`ModuleLocation`] is the **namespace prefix a qualified name is spelled
//! with**. A `Package` entity is a **declaration**, and only the file that *is* a package's root
//! module emits one. Overloading the two is what produced the defect above: `crates/core/flags/
//! defs.rs` was given the package `flags` because that is the directory above it, and the module
//! table then held a namespace called `flags` in which `crate::flags::Flag` could not be spelled,
//! while `crates/matcher/tests/util.rs` and `tests/util.rs` were both the module `tests::util`.
//! The prefix stays an approximation and says so; the declaration is a fact and is stored as a
//! row. See [`for_file`] for where the boundary is written.
//!
//! # The crate boundary
//!
//! **Decision: the boundary is an `EntityKind::Package` entity plus a `Contains` edge to the
//! package's root module.** The argument is at [`for_file`].

use crate::extract::spec::{LanguageSpec, ModuleLayout};
use crate::model::{
    Entity, EntityId, EntityKind, Evidence, Language, Relation, RelationKind, RepoPath, Span,
};
use std::collections::BTreeSet;

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

/// The directories a repository roots its packages at.
///
/// **This is a repository-level fact and it is here because per-file extraction is the only thing
/// that runs.** Which files share a package is a property of the *set* of paths: nothing in
/// `crates/core/flags/defs.rs` distinguishes it from a file in a crate that genuinely is called
/// `flags`, because in both cases the path is `crates`/`core`/`flags`/`defs.rs` and only the file
/// beside it says otherwise. A rule that cannot see the file set therefore has to name the file
/// after the directory above it, which is what it used to do, and that put thirty files of one
/// crate into thirty namespaces.
///
/// # What counts as a package root
///
/// A directory holds a root module in one of the two shapes a language's layout declares:
///
/// * **directly** — `crates/core/main.rs`, which is how a crate with no source root is laid out;
/// * **through a source root** — `crates/foo/src/lib.rs`, where the source root is a directory
///   *inside* the package rather than the package's own boundary.
///
/// Those are the only two shapes [`ModuleLayout::package_roots`] and
/// [`ModuleLayout::source_roots`] together describe, so they are the only two this counts. A
/// deeper `src/sub/lib.rs` is not a package root: `src/sub` is a module of `foo`, and the nearest
/// rule would otherwise have named a package `sub` and lost the outer one.
///
/// # Nearest, and why it is not outermost
///
/// **A nested package is a separate namespace.** `cargo` has `crates/*`, `regex` has
/// `crates/*/regex-*`, and ripgrep has `crates/core` inside the `ripgrep` workspace. Taking the
/// outermost package root would merge every one of those into the workspace root package, so
/// `alpha::gateway` and `beta::service` would become one namespace and every cross-crate path in a
/// workspace would be unspellable — which is the only thing this table is for. Taking the
/// nearest keeps them apart, and costs nothing when there is no nesting: `crates/foo/src/a.rs`
/// has one package root either way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageRoots {
    /// Repository-relative directories, `/`-joined. A root-level file's directory is the empty
    /// string, which is a real directory and is not the same as an absent one.
    roots: BTreeSet<String>,
}

impl PackageRoots {
    /// No package roots, which is what a caller that knows nothing about the repository has.
    ///
    /// Every file then falls back to its own directory, so this is the pre-existing behaviour
    /// rather than a degraded mode: `extract_with` is the single-file entry point and passes one
    /// of these.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Read the package roots out of a repository's paths.
    ///
    /// One pass per layout. Paths for other languages are harmless here rather than filtered:
    /// a directory only enters the set when a file in it is named for a root stem, so a
    /// JavaScript `index.js` cannot make a Rust `lib.rs` appear.
    pub fn from_paths<I>(paths: I, layout: ModuleLayout) -> Self
    where
        I: IntoIterator<Item = RepoPath>,
    {
        let mut holders: BTreeSet<String> = BTreeSet::new();
        for path in paths {
            let (Some(stem), Some(directory)) = (stem_of(&path), directory_of(&path)) else {
                continue;
            };
            if layout.package_roots.contains(&stem) {
                holders.insert(directory.to_owned());
            }
        }

        let mut roots = BTreeSet::new();
        for holder in holders {
            // The directory that holds the root module *is* the package root, unless the root
            // module is inside a source root — in which case the package root is the directory
            // holding *that*, and the source root is not a package of its own.
            roots.insert(holder.clone());
            let mut components: Vec<&str> = holder.split('/').collect();
            if components.pop().is_some_and(|name| layout.source_roots.contains(&name)) {
                let above = components.join("/");
                roots.insert(above);
            }
        }
        Self { roots }
    }

    /// Whether `directory` — repository-relative, `/`-joined — holds a package's root module.
    pub fn contains(&self, directory: &str) -> bool {
        self.roots.contains(directory)
    }

    /// How many package roots were found. Zero means every file falls back to its own directory.
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    /// Whether no package root was found at all.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The package roots, for a caller that wants to print or assert on them.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.roots.iter().map(String::as_str)
    }
}

/// Every language's package roots, from one pass over a repository's paths.
///
/// The extractor is called per file with a spec, and the package roots depend on the layout, so
/// the indexer needs one set per language rather than one set for the repository. There is one
/// language with a layout today, so this is a list of one and the shape is a list because the
/// table is per language by construction.
#[derive(Debug, Clone, Default)]
pub struct RepositoryLayout {
    /// Handed back for a language that has no module layout, so a caller never has to invent an
    /// empty set of its own.
    none: PackageRoots,
    per_language: Vec<(Language, ModuleLayout, PackageRoots)>,
}

impl RepositoryLayout {
    /// A layout that knows nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The package roots for `spec`'s language, or the empty set when it has no module layout.
    pub fn roots(&self, spec: &LanguageSpec) -> &PackageRoots {
        self.per_language
            .iter()
            .find(|(language, _, _)| *language == spec.language)
            .map(|(_, _, roots)| roots)
            .unwrap_or(&self.none)
    }

    /// Read one path into every layout that has one. Cheap enough to call per file: the set of
    /// layouts is the set of languages the registry declares a module layout for.
    pub fn observe(&mut self, path: &RepoPath) {
        if self.per_language.is_empty() {
            self.per_language = crate::extract::registry::all()
                .iter()
                .filter_map(|spec| {
                    Some((spec.language, spec.module_layout()?, PackageRoots::default()))
                })
                .collect();
        }
        let (Some(stem), Some(directory)) = (stem_of(path), directory_of(path)) else {
            return;
        };
        for (_, layout, roots) in &mut self.per_language {
            if !layout.package_roots.contains(&stem) {
                continue;
            }
            roots.roots.insert(directory.to_owned());
            let mut components: Vec<&str> = directory.split('/').collect();
            if components.pop().is_some_and(|name| layout.source_roots.contains(&name)) {
                let above = components.join("/");
                roots.roots.insert(above);
            }
        }
    }

    /// Read a repository's paths, in whatever order the caller already has them.
    pub fn observe_all<'a, I>(&mut self, paths: I)
    where
        I: IntoIterator<Item = &'a RepoPath>,
    {
        for path in paths {
            self.observe(path);
        }
    }
}

/// Whether any of these paths would consult the repository's package roots at all.
///
/// **A path that contains a source root never does**, because the source root says where the
/// package's own tree begins and the rule for that case reads nothing but the path. A caller that
/// only needs this for some of its files uses it to decide whether building the set is worth the
/// read at all — which is what keeps an incremental refresh of a `src/` file proportional to that
/// one file.
pub fn needs_package_roots<'a, I>(paths: I, layout: ModuleLayout) -> bool
where
    I: IntoIterator<Item = &'a RepoPath>,
{
    paths.into_iter().any(|path| {
        !path
            .components()
            .any(|component| layout.source_roots.contains(&component))
    })
}

/// The directory a path is written in, `""` for a file at the repository root.
fn directory_of(path: &RepoPath) -> Option<&str> {
    path.as_str().rfind('/').map(|slash| &path.as_str()[..slash])
}

/// A file's stem: everything before its extension, or the whole name for a dotfile.
///
/// Shared with [`locate`] so the two cannot disagree about what `mod.rs` and `.gitignore` are.
fn stem_of(path: &RepoPath) -> &str {
    let name = path.file_name();
    match name.rsplit_once('.') {
        Some((head, _)) if !head.is_empty() => head,
        // A dotfile has no extension, so `.gitignore` is a stem rather than an empty one.
        _ => name,
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
///    property of the *set* of paths, and per-file extraction is the only thing that runs. The
///    set arrives as a [`PackageRoots`], and a caller that has none — [`crate::extract::
///    extract_with`], which sees one file — gets the fallback naming rather than a wrong boundary.
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
    roots: &PackageRoots,
) -> FileModules {
    let Some(layout) = spec.module_layout() else {
        return FileModules::default();
    };
    let location = locate(path, &layout, roots);
    let qualified = location.qualified_name(layout.segment_separator);
    let name = location.name();
    let fingerprint = fingerprint(text);

    let mut entities = Vec::new();
    let mut relations = Vec::new();

    // The declaration, and only here. **A package entity is emitted by the file that *is* the
    // package's root module and by no other file in the package**, which is what keeps "the
    // package this file's qualified name is prefixed with" and "a package exists here" as two
    // facts. Naming every file after its own directory — which is what a path alone can do — makes
    // the two the same fact, and then `crates/core/flags/defs.rs` declares a package called
    // `flags` in a directory that holds no crate root, and `tests/index/basic.rs` declares a
    // package called `index` beside the real one.
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
/// Pure: a path in, a name out, given the repository's package roots. Every approximation this
/// makes is listed in the module documentation, and each one is pinned by a test here so it
/// cannot drift silently.
///
/// `roots` is [`PackageRoots::empty`] for a caller that knows nothing about the repository, and
/// every file then falls back to its own directory. That is the behaviour this function had
/// before it took the set, so the single-file entry point is unchanged rather than degraded.
pub fn locate(path: &RepoPath, layout: &ModuleLayout, roots: &PackageRoots) -> ModuleLocation {
    let components: Vec<&str> = path.components().collect();
    let (file_name, directories) = match components.split_last() {
        Some((last, rest)) => (*last, rest),
        // `RepoPath` rejects an empty path, so a one-component path is the shortest case and
        // this branch is unreachable. Falling back to the file's own name keeps the function
        // total rather than panicking over an invariant three files away.
        None => (path.as_str(), &[] as &[&str]),
    };
    let stem = stem_of(path);

    // The first source root in the path is the package's own source tree. A nested `src` further
    // down is a directory that happens to be called `src`. Where there is one, the path states
    // where the package begins and nothing else is consulted.
    let source_root = directories
        .iter()
        .position(|component| layout.source_roots.contains(component));

    // The package is the nearest ancestor directory that holds a package's root module, and the
    // module's segments are everything between that directory and the file.
    let (package, first_segment) = match source_root {
        Some(index) => {
            // The directory holding the source root, or the source root's own name when the
            // source root *is* the repository root — there is no directory above it to take a
            // name from, and the approximation is visible in every qualified name it produces.
            let holder = directories[..index]
                .last()
                .copied()
                .unwrap_or(directories[index]);
            (normalise_package_name(holder), index + 1)
        }
        None => package_without_a_source_root(directories, roots, stem),
    };
    // `first_segment` is either an index one past the package directory or `directories.len()`,
    // and `directories[first_segment..]` is a legal slice in both cases.
    let below = &directories[first_segment.min(directories.len())..];

    let is_package_root = below.is_empty() && layout.package_roots.contains(&stem);

    // `a/mod.rs` is the module `a`: the stem names the directory, so it is not a segment. That
    // only works when there *is* a directory below the source root; a bare `src/mod.rs` has
    // nothing to take a name from, and treating it as the package root module would give it the
    // same name as `src/lib.rs` and put two different files in one namespace.
    let names_its_directory = layout.directory_modules.contains(&stem) && !below.is_empty();
    // `below` borrows from the path's components and `stem` is owned, so the segments are
    // collected into `String`s in both branches. Mixing the two would make the field's type depend
    // on which branch ran, which is the kind of thing that compiles in one configuration and not
    // another.
    let segments: Vec<String> = match is_package_root || names_its_directory {
        true => below.iter().map(|part| (*part).to_owned()).collect(),
        false => below
            .iter()
            .map(|part| (*part).to_owned())
            .chain(std::iter::once(stem.to_owned()))
            .collect(),
    };

    ModuleLocation {
        package,
        segments,
        is_package_root,
    }
}

/// The package of a file whose path names no source root, and the first segment below it.
///
/// **The nearest ancestor that directly holds a root module**, which is the only thing in this
/// case that identifies a package: with no source root the path says nothing about where the
/// crate begins, so the file set is the only evidence, and it is the directory holding the root
/// module that is the evidence.
///
/// The repository root is not an answer even when it holds a root module: its name is the
/// directory the checkout happens to sit in, `EntityId` is two thirds `qualified_name`, and a name
/// that depends on where the repository was cloned makes the same repository two different graphs.
/// The fallback below is visibly wrong instead, which is the trade this makes deliberately.
fn package_without_a_source_root(
    directories: &[&str],
    roots: &PackageRoots,
    stem: &str,
) -> (String, usize) {
    for index in (0..directories.len()).rev() {
        let directory = directories[..=index].join("/");
        if !roots.contains(&directory) {
            continue;
        }
        let name = directories[index];
        if !name.is_empty() {
            return (normalise_package_name(name), index + 1);
        }
    }
    // Nothing above the file holds a root module. The file's own directory is the only thing
    // left, and an empty package would make every qualified name unmatchable.
    match directories.last() {
        Some(name) => (normalise_package_name(name), directories.len()),
        None => (normalise_package_name(stem), 0),
    }
}

/// Cargo's own rule: a package directory named `regex-automata` is a package named
/// `regex_automata`, which is also the only spelling a `use` statement can say.
fn normalise_package_name(directory: &str) -> String {
    directory.replace('-', "_")
}

/// A structural fingerprint of a module: FNV-1a over its source text, hex-encoded.
///
/// Deliberately not a cryptographic hash. This is a reconciliation aid — "these two entities
/// have the same body" — and it is checked against the algorithm's published test vectors, which
/// is what makes it verifiable without a dependency. A file module hashes the whole file and an
/// inline `mod x { .. }` hashes its own node text; a `mod x;` hashes those six characters, which
/// is not a body but is stable under every edit that does not touch the declaration, and is
/// better than a fingerprint that silently means "this entity was never examined".
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

/// Read a committed fixture under `tests/fixtures/modules/`.
///
/// Panics when the file is missing rather than returning an empty string. A test that reads a
/// fixture which is not committed passes for the wrong reason, which is the same defect as a
/// test asserting on a file that does not exist — the `touponly`/`toponly` bug in this
/// project's own discovery tests.
#[cfg(test)]
pub(crate) fn fixture_source(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("modules")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "fixture {} could not be read: {error}. It has to be committed, and the whole tree \
             is force-added because the verification sandbox runs `git clean -fdx`.",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{ModuleLayout, fingerprint, fixture_source, for_file, locate};
    use crate::extract::registry;
    use crate::model::{EntityKind, RelationKind, ResolutionState, Span};

    /// Every fixture file these tests read, in one table.
    ///
    /// Three lists used to be three copies of the same names, and they could disagree: a path in
    /// one and not another is a file on disk that some test reads and some test does not know
    /// about. This is the one list, and [`fixture_paths`] builds a real package-root set from it so
    /// a fixture test reads the same roots a full build of that tree would.
    const NAMED_FIXTURES: &[&str] = &[
        "nested/src/lib.rs",
        "nested/src/outer/mod.rs",
        "nested/src/outer/middle/mod.rs",
        "nested/src/outer/middle/inner.rs",
        "several/src/lib.rs",
        "several/src/a.rs",
        "several/src/b.rs",
        "several/src/c.rs",
        "cross/alpha/src/lib.rs",
        "cross/alpha/src/gateway.rs",
        "cross/beta/src/lib.rs",
        "cross/beta/src/service.rs",
        "reexport/src/lib.rs",
        "reexport/src/inner/mod.rs",
        "reexport/src/inner/thing.rs",
        "reexport/src/inner/helpers.rs",
        "nosrc/core/main.rs",
        "nosrc/core/search.rs",
        "nosrc/core/flags/mod.rs",
        "nosrc/core/flags/defs.rs",
        "nosrc/core/flags/complete/mod.rs",
        "nosrc/core/flags/complete/bash.rs",
        "nestedpkg/outer/src/lib.rs",
        "nestedpkg/outer/src/a.rs",
        "nestedpkg/outer/inner/main.rs",
        "nestedpkg/outer/inner/mod.rs",
        "nestedpkg/outer/inner/b.rs",
    ];

    /// Assert that every fixture this test module names is actually on disk.
    ///
    /// A table of paths nothing checks is a list of intentions. This is what turns it into a list
    /// of files, and it fails as a *missing file* rather than as a puzzling assertion further
    /// down.
    #[test]
    fn every_module_fixture_named_by_these_tests_is_committed() {
        let mut named: Vec<&str> = NAMED_FIXTURES.to_vec();
        named.push("README.md");
        let empty: Vec<&str> = named
            .iter()
            .copied()
            .filter(|relative| fixture_source(relative).is_empty())
            .collect();
        assert!(
            empty.is_empty(),
            "fixtures that are committed but empty: {empty:?}"
        );
    }

    fn path(s: &str) -> crate::model::RepoPath {
        crate::model::RepoPath::new(s).expect("valid path")
    }

    /// The Rust layout, read from the registry rather than restated, so a test cannot pass
    /// against a table the extractor no longer uses.
    fn rust_layout() -> ModuleLayout {
        registry::get(crate::model::Language::Rust)
            .expect("rust spec")
            .module_layout()
            .expect("rust declares a module layout")
    }

    fn span() -> Span {
        Span::new(0, 10, 1, 1, 1, 11).expect("valid span")
    }

    fn file_id(p: &str) -> crate::model::EntityId {
        crate::model::EntityId::new(path(p), EntityKind::File, "x.rs", 0)
    }

    /// No package roots, which is what a caller that knows nothing about the repository has.
    fn no_roots() -> super::PackageRoots {
        super::PackageRoots::empty()
    }

    fn roots_of(paths: &[&str]) -> super::PackageRoots {
        super::PackageRoots::from_paths(
            paths.iter().map(|p| path(p)).collect::<Vec<_>>(),
            rust_layout(),
        )
    }

    fn modules_for(p: &str) -> super::FileModules {
        modules_among(p, &no_roots())
    }

    fn modules_among(p: &str, roots: &super::PackageRoots) -> super::FileModules {
        let spec = registry::get(crate::model::Language::Rust).expect("rust spec");
        for_file(spec, &path(p), &file_id(p), span(), "fn a() {}", roots)
    }

    fn module_names(p: &str) -> Vec<String> {
        modules_for(p)
            .entities
            .iter()
            .filter(|entity| entity.kind() == EntityKind::Module)
            .map(|entity| entity.id.qualified_name().to_owned())
            .collect()
    }

    fn located(p: &str, roots: &super::PackageRoots) -> super::ModuleLocation {
        locate(&path(p), &rust_layout(), roots)
    }

    /// Every path in the committed fixture trees, so a fixture test reads the same package roots a
    /// full build of that tree would.
    fn fixture_paths() -> Vec<crate::model::RepoPath> {
        NAMED_FIXTURES
            .iter()
            .map(|relative| path(relative))
            .collect()
    }

    fn fixture_roots() -> super::PackageRoots {
        super::PackageRoots::from_paths(fixture_paths(), rust_layout())
    }

    #[test]
    fn a_source_file_is_the_module_its_path_names() {
        // The whole reason this file exists. `use regex_automata::util::look::Matcher;` can only
        // be a lookup if something in the index is called `regex_automata::util::look`.
        assert_eq!(
            module_names("crates/regex-automata/src/util/look.rs"),
            vec!["regex_automata::util::look".to_owned()]
        );
    }

    #[test]
    fn a_mod_rs_file_is_the_module_its_directory_names() {
        // Rust has no separate directory entity: `a/mod.rs` *is* the module `a`, and the `mod`
        // in its filename is a convention rather than part of the name.
        assert_eq!(
            module_names("crates/regex-automata/src/util/mod.rs"),
            vec!["regex_automata::util".to_owned()]
        );
    }

    #[test]
    fn a_crate_root_file_is_the_module_named_for_its_package() {
        // `lib` is a filename convention. Nothing can write `use lib::…`, so naming the module
        // `lib` would be a name the source can never produce.
        assert_eq!(
            module_names("crates/regex/src/lib.rs"),
            vec!["regex".to_owned()]
        );
    }

    #[test]
    fn a_crate_root_file_is_the_only_one_that_names_a_package() {
        // `src/a/main.rs` is the module `pkg::a::main`. Treating it as a second package because
        // its stem is `main` would split one package's namespace into two.
        let roots = no_roots();
        assert!(
            !locate(&path("crates/regex/src/a/main.rs"), &rust_layout(), &roots).is_package_root,
            "a main.rs below the source root is a module, not a package"
        );
        assert_eq!(
            locate(&path("crates/regex/src/a/main.rs"), &rust_layout(), &roots).qualified_name("::"),
            "regex::a::main"
        );
        assert!(
            locate(&path("crates/regex/src/lib.rs"), &rust_layout(), &roots).is_package_root,
            "a lib.rs directly in the source root is the package root"
        );
    }

    #[test]
    fn a_nested_source_directory_is_not_a_second_package_root() {
        // `src` is a directory name, not a reserved word. Taking the *first* one keeps every
        // file under it in one namespace; taking the last would give `inner` its own.
        let layout = rust_layout();
        let inner = locate(&path("crates/foo/src/inner/src/look.rs"), &layout, &no_roots());
        assert_eq!(inner.package, "foo");
        assert_eq!(
            inner.qualified_name("::"),
            "foo::inner::src::look",
            "a nested `src` is an ordinary directory segment"
        );
    }

    #[test]
    fn a_package_name_is_spelled_the_way_a_use_statement_spells_it() {
        // Cargo replaces a hyphen with an underscore in the package name, and that underscore is
        // the only spelling a `use` statement can have. Keeping the hyphen would make every
        // cross-crate module name unmatchable.
        let layout = rust_layout();
        let located = locate(&path("crates/regex-automata/src/lib.rs"), &layout, &no_roots());
        assert_eq!(located.package, "regex_automata");
    }

    #[test]
    fn a_package_at_the_repository_root_is_named_after_its_source_root_and_says_so() {
        // The one approximation that produces a name which is simply wrong. It is pinned here so
        // that a future `Cargo.toml` reader can see exactly which case it has to fix, and so that
        // nobody discovers the behaviour from a qualified name in a report.
        let layout = rust_layout();
        let located = locate(&path("src/lib.rs"), &layout, &no_roots());
        assert_eq!(located.package, "src");
        assert!(located.is_package_root);
    }

    #[test]
    fn a_file_with_no_source_root_above_it_still_gets_a_module() {
        // Refusing to emit a module would leave every non-Cargo Rust file in the index without a
        // namespace, which is the same silent gap this whole change exists to close. The package
        // name is wrong here and there is no honest way to do better from a path alone.
        let layout = rust_layout();
        let located = locate(&path("tests/helper.rs"), &layout, &no_roots());
        assert_eq!(located.qualified_name("::"), "tests::helper");
    }

    #[test]
    fn a_mod_rs_directly_in_the_source_root_does_not_take_the_crate_root_s_name() {
        // `a/mod.rs` is the module `a` because there is a directory called `a`. A bare
        // `src/mod.rs` has no such directory, and naming it after the package would put two
        // different files in one namespace under one name.
        let layout = rust_layout();
        let located = locate(&path("crates/foo/src/mod.rs"), &layout, &no_roots());
        assert_eq!(located.qualified_name("::"), "foo::mod");
        assert!(!located.is_package_root);
        assert_ne!(
            located.qualified_name("::"),
            locate(&path("crates/foo/src/lib.rs"), &layout, &no_roots()).qualified_name("::"),
        );
    }

    #[test]
    fn a_file_at_the_repository_root_falls_back_to_its_own_name() {
        // A file at the very top of a checkout has no directory above its source root and no
        // directory of its own. Something has to be chosen, and an empty package would make
        // every qualified name unmatchable.
        let layout = rust_layout();
        let located = locate(&path("toponly.rs"), &layout, &no_roots());
        assert_eq!(located.package, "toponly");
        assert_eq!(located.qualified_name("::"), "toponly::toponly");
        assert!(!located.is_package_root, "and it declares no package");
    }

    #[test]
    fn a_module_qualified_name_starts_with_the_package_even_when_there_are_no_segments() {
        let layout = rust_layout();
        let located = locate(&path("crates/regex/src/lib.rs"), &layout, &no_roots());
        assert!(located.segments.is_empty());
        assert_eq!(located.name(), "regex");
        assert_eq!(located.qualified_name("::"), "regex");
    }

    #[test]
    fn a_module_is_contained_in_the_file_that_writes_it() {
        // The file entity and the module entity are different rows answering different questions,
        // and without this edge nothing in the graph relates them.
        let found = modules_for("crates/regex/src/lib.rs");
        let contains = found
            .relations
            .iter()
            .find(|relation| relation.kind == RelationKind::Contains)
            .expect("the file contains the module");
        assert_eq!(contains.source, file_id("crates/regex/src/lib.rs"));
        assert_eq!(contains.target, found.module);
        assert_eq!(contains.target_name, "regex");
    }

    #[test]
    fn a_containment_edge_is_settled_by_grammar_and_never_re_decided() {
        // The crate boundary is a fact about the build, not a claim about a name. A boundary
        // stored as an inference would be re-decided on every refresh and could come back
        // ambiguous after an unrelated edit.
        let found = modules_for("crates/regex/src/lib.rs");
        let boundaries: Vec<_> = found
            .relations
            .iter()
            .filter(|relation| relation.source.kind() == EntityKind::Package)
            .collect();
        assert_eq!(boundaries.len(), 1, "{found:#?}");
        assert_eq!(
            boundaries[0].resolution,
            ResolutionState::Resolved {
                by: crate::model::Evidence::Containment
            }
        );
        assert!(
            !RelationKind::Contains.is_dependency(),
            "a containment edge is structure, not a dependency, so it stays out of every \
             dependency traversal"
        );
    }

    #[test]
    fn only_a_package_root_file_emits_a_package_entity() {
        assert!(
            modules_for("crates/regex/src/lib.rs")
                .entities
                .iter()
                .any(|entity| entity.kind() == EntityKind::Package),
            "the crate root is where the package is declared"
        );
        assert!(
            !modules_for("crates/regex/src/automata/mod.rs")
                .entities
                .iter()
                .any(|entity| entity.kind() == EntityKind::Package),
            "a module file belongs to a package it does not declare"
        );
    }

    #[test]
    fn a_module_covers_the_whole_file_it_is_written_in() {
        // A caller that asks "where is this module" gets the file, and a module with a two-byte
        // span would say the module is the first two bytes of the file.
        let found = modules_for("crates/regex/src/lib.rs");
        let module = found
            .entities
            .iter()
            .find(|entity| entity.kind() == EntityKind::Module)
            .expect("a module entity");
        assert_eq!(module.span, Some(span()));
    }

    #[test]
    fn a_module_fingerprints_its_own_text_so_a_moved_module_is_still_the_same_one() {
        let first = modules_for("crates/regex/src/lib.rs");
        let again = modules_for("crates/regex/src/lib.rs");
        let before = first
            .entities
            .iter()
            .find(|entity| entity.kind() == EntityKind::Module)
            .and_then(|entity| entity.structural_fingerprint.clone());
        let after = again
            .entities
            .iter()
            .find(|entity| entity.kind() == EntityKind::Module)
            .and_then(|entity| entity.structural_fingerprint.clone());
        assert_eq!(before, after);
        assert!(
            before
                .as_deref()
                .is_some_and(|value| value.starts_with("fnv1a64:")),
            "a module body can be hashed, so the fingerprint is filled in: {before:?}"
        );
    }

    #[test]
    fn the_fingerprint_matches_the_published_fnv1a_vectors() {
        // An unreferenced hash is an unverifiable one. These three are the values the algorithm's
        // own specification publishes for the 64-bit variant, so the function is checked against
        // something outside this repository rather than against itself.
        assert_eq!(fingerprint(""), "fnv1a64:cbf29ce484222325");
        assert_eq!(fingerprint("a"), "fnv1a64:af63dc4c8601ec8c");
        assert_eq!(fingerprint("foobar"), "fnv1a64:85944171f73967e8");
    }

    #[test]
    fn a_language_with_no_module_layout_contributes_nothing() {
        // The honest answer for a language whose file-to-module convention is unknown. The
        // engine Peek replaces derived a module from any string containing a dot, a slash or a
        // hyphen, and a kebab-cased JavaScript identifier matched (audit B7).
        let without = crate::extract::spec::LanguageSpec {
            language: crate::model::Language::Rust,
            symbols: &[],
            calls: &[],
            imports: &[],
            inheritance: None,
            references: None,
            scope_nodes: &[],
            type_scope_nodes: &[],
            module_nodes: &[],
            modules: None,
            grammar: || tree_sitter_rust::LANGUAGE.into(),
        };
        let found = for_file(
            &without,
            &path("crates/regex/src/lib.rs"),
            &file_id("crates/regex/src/lib.rs"),
            span(),
            "fn a() {}",
        );
        assert!(found.is_empty(), "{found:#?}");
        assert!(found.module.is_none());
    }

    // -----------------------------------------------------------------------
    // The committed trees
    //
    // Everything above proves the rule against paths written out by hand. These read real files
    // out of `tests/fixtures/modules/`, so the rule is also proved against source somebody could
    // compile.
    // -----------------------------------------------------------------------

    /// The module and package entities one committed fixture file contributes.
    ///
    /// Read with the **whole fixture tree's** package roots, because a full build of that tree
    /// would have them, and a fixture tree is a repository like any other: `nosrc/core/main.rs`
    /// is what tells the extractor that `nosrc/core/flags/defs.rs` is in the package `core`.
    fn fixture_modules(relative: &str) -> Vec<String> {
        let found = for_file(
            registry::get(crate::model::Language::Rust).expect("rust spec"),
            &path(relative),
            &file_id(relative),
            span(),
            &fixture_source(relative),
            &fixture_roots(),
        );
        let mut names: Vec<String> = found
            .entities
            .iter()
            .map(|entity| format!("{} {}", entity.kind().as_str(), entity.id.qualified_name()))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_three_level_nested_tree_names_every_level_from_the_crate_root() {
        // The shape `mod.rs` files exist for. Reading `mod outer;` as the module `outer` and
        // stopping there would put every level below it outside any namespace at all.
        assert_eq!(
            fixture_modules("nested/src/lib.rs"),
            vec!["module nested".to_owned(), "package nested".to_owned()]
        );
        assert_eq!(
            fixture_modules("nested/src/outer/mod.rs"),
            vec!["module nested::outer".to_owned()]
        );
        assert_eq!(
            fixture_modules("nested/src/outer/middle/mod.rs"),
            vec!["module nested::outer::middle".to_owned()]
        );
        assert_eq!(
            fixture_modules("nested/src/outer/middle/inner.rs"),
            vec!["module nested::outer::middle::inner".to_owned()]
        );
    }

    #[test]
    fn a_crate_with_several_modules_names_each_one_from_the_crate_root() {
        assert_eq!(
            fixture_modules("several/src/lib.rs"),
            vec!["module several".to_owned(), "package several".to_owned()]
        );
        assert_eq!(
            fixture_modules("several/src/a.rs"),
            vec!["module several::a".to_owned()]
        );
        assert_eq!(
            fixture_modules("several/src/b.rs"),
            vec!["module several::b".to_owned()]
        );
        assert_eq!(
            fixture_modules("several/src/c.rs"),
            vec!["module several::c".to_owned()]
        );
    }

    #[test]
    fn a_module_declaration_and_the_file_that_defines_it_are_two_different_entities() {
        // `mod a;` in `several/src/lib.rs` and `several/src/a.rs` describe one module from two
        // sides. Collapsing them would lose the site that declared it; giving the declaration
        // the same name as the file would lose the file it lives in.
        let root = crate::extract::walker::extract_with_roots(
            registry::get(crate::model::Language::Rust).expect("rust spec"),
            path("several/src/lib.rs"),
            &fixture_source("several/src/lib.rs"),
            &fixture_roots(),
        );
        assert!(root.is_clean(), "{:?}", root.degradation);
        let declaration = root
            .entities
            .iter()
            .find(|entity| {
                entity.kind() == EntityKind::Module && !root.module_ids.contains(&entity.id)
            })
            .expect("`mod a;` declared a module entity in the file that declares it");
        assert_eq!(declaration.id.path().as_str(), "several/src/lib.rs");
        assert_eq!(declaration.id.qualified_name(), "a");
        assert_eq!(
            fixture_modules("several/src/a.rs"),
            vec!["module several::a".to_owned()],
            "and the file it names is a different entity, named for the path it is written at"
        );
    }

    #[test]
    fn two_packages_in_one_tree_get_two_names_and_no_shared_namespace() {
        // The cross-crate case that dominated the measured decide rate. `use alpha::gateway::…`
        // in the `beta` package is only a lookup if `alpha::gateway` exists as a name, and it is
        // only distinct from `beta`'s own modules because the package prefix is in the name.
        assert_eq!(
            fixture_modules("cross/alpha/src/lib.rs"),
            vec!["module alpha".to_owned(), "package alpha".to_owned()]
        );
        assert_eq!(
            fixture_modules("cross/alpha/src/gateway.rs"),
            vec!["module alpha::gateway".to_owned()]
        );
        assert_eq!(
            fixture_modules("cross/beta/src/lib.rs"),
            vec!["module beta".to_owned(), "package beta".to_owned()]
        );
        assert_eq!(
            fixture_modules("cross/beta/src/service.rs"),
            vec!["module beta::service".to_owned()]
        );
    }

    #[test]
    fn a_re_export_tree_names_the_module_it_publishes_part_of() {
        assert_eq!(
            fixture_modules("reexport/src/lib.rs"),
            vec!["module reexport".to_owned(), "package reexport".to_owned()]
        );
        assert_eq!(
            fixture_modules("reexport/src/inner/mod.rs"),
            vec!["module reexport::inner".to_owned()]
        );
        assert_eq!(
            fixture_modules("reexport/src/inner/thing.rs"),
            vec!["module reexport::inner::thing".to_owned()]
        );
        assert_eq!(
            fixture_modules("reexport/src/inner/helpers.rs"),
            vec!["module reexport::inner::helpers".to_owned()]
        );
    }

    #[test]
    fn a_fixture_under_this_repository_produces_the_same_module_name_as_the_bare_tree() {
        // The rule reads a repository-relative path, and the fixtures are read through a path
        // relative to the fixture directory. If the two ever disagreed, the fixtures would be
        // proving something the indexer would never do.
        let layout = rust_layout();
        let roots = no_roots();
        let bare = locate(&path("cross/alpha/src/gateway.rs"), &layout, &roots);
        let in_repository = locate(
            &path("crates/peek-core/tests/fixtures/modules/cross/alpha/src/gateway.rs"),
            &layout,
            &roots,
        );
        assert_eq!(bare, in_repository);
        assert_eq!(in_repository.qualified_name("::"), "alpha::gateway");
    }

    // -----------------------------------------------------------------------
    // A crate with no `src/`
    //
    // The layout that fails. Every `src/` case above reads the path alone; every case here needs
    // to know which ancestor directory holds a crate root, and the only thing that says is the
    // file set.
    // -----------------------------------------------------------------------

    #[test]
    fn a_crate_with_no_source_root_names_every_file_from_its_package_root() {
        // The defect, stated as a test. `nosrc/core/main.rs` is the crate root, so every file
        // under `nosrc/core/` is in the package `core` — and `nosrc/core/flags/defs.rs` is the
        // module `core::flags::defs`. Naming that file after the directory above it gives the
        // package `flags` and the module `flags::defs`, and then the `use crate::flags::Flag;` in
        // `nosrc/core/main.rs` cannot be spelled: the table would be asked for
        // `flags::flags::Flag`, and the bare `flags::defs` it falls back to is a module in a
        // package called `flags` that does not exist.
        let roots = fixture_roots();
        assert_eq!(
            located("nosrc/core/main.rs", &roots).qualified_name("::"),
            "core"
        );
        assert_eq!(
            located("nosrc/core/search.rs", &roots).qualified_name("::"),
            "core::search"
        );
        assert_eq!(
            located("nosrc/core/flags/defs.rs", &roots).qualified_name("::"),
            "core::flags::defs",
            "two directories below the crate root and one package"
        );
        assert_eq!(
            located("nosrc/core/flags/mod.rs", &roots).qualified_name("::"),
            "core::flags",
            "and the module its directory names is reached through the same package"
        );
        assert_eq!(
            located("nosrc/core/flags/complete/mod.rs", &roots)
                .qualified_name("::"),
            "core::flags::complete",
            "four levels down, still one package"
        );
        assert_eq!(
            located("nosrc/core/flags/complete/bash.rs", &roots)
                .qualified_name("::"),
            "core::flags::complete::bash"
        );
    }

    #[test]
    fn a_crate_with_no_source_root_still_declares_exactly_one_package() {
        // **The package the name is prefixed with and the declaration that a package exists are
        // two facts, and only the root module file declares one.** Every file in the crate gets
        // the package `core` in its module name; only `nosrc/core/main.rs` emits a `Package`
        // entity. Emitting one per directory — which is what naming a file after its own
        // directory implies — would declare a package called `flags` in a directory holding no
        // crate root.
        let roots = fixture_roots();
        for relative in [
            "nosrc/core/main.rs",
            "nosrc/core/search.rs",
            "nosrc/core/flags/mod.rs",
            "nosrc/core/flags/defs.rs",
            "nosrc/core/flags/complete/mod.rs",
            "nosrc/core/flags/complete/bash.rs",
        ] {
            let packages: Vec<String> = modules_among(relative, &roots)
                .entities
                .iter()
                .filter(|entity| entity.kind() == EntityKind::Package)
                .map(|entity| entity.id.qualified_name().to_owned())
                .collect();
            assert_eq!(
                packages,
                if relative == "nosrc/core/main.rs" {
                    vec!["core".to_owned()]
                } else {
                    Vec::new()
                },
                "{relative} declares the wrong packages"
            );
        }
    }

    #[test]
    fn a_directory_that_is_a_module_holds_no_package_root_and_no_crate_root_beside_it() {
        // **The two facts are kept apart by only one thing: the `Package` entity is emitted by the
        // file that *is* a root module.** `nosrc/core/flags` is a module directory — it has a
        // `mod.rs` — and it holds no root module, so nothing in it declares a package. If the
        // package name on a module row were read as "a package is declared here", this
        // directory would declare one.
        let roots = fixture_roots();
        let flags = located("nosrc/core/flags/mod.rs", &roots);
        assert_eq!(flags.package, "core");
        assert!(!flags.is_package_root);
        let defs = located("nosrc/core/flags/defs.rs", &roots);
        assert_eq!(defs.package, "core");
        assert!(!defs.is_package_root);
        assert!(
            !roots.contains("nosrc/core/flags"),
            "a directory holding a `mod.rs` is a module, not a package root: {:?}",
            roots.iter().collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // A nested package
    // -----------------------------------------------------------------------

    #[test]
    fn a_nested_package_takes_the_nearest_package_root_and_not_the_outermost_one() {
        // `nestedpkg/outer` is a package because it has a `src/lib.rs`, and
        // `nestedpkg/outer/inner` is a package because it has a `main.rs`. **Nearest wins**, and
        // the measurement is the reason: taking the outermost would put `outer::a` and
        // `inner::b` in one namespace, so a cross-crate `use inner::b::Thing;` from `outer`
        // becomes a path no table can answer, which is the whole of what the table is for.
        let roots = fixture_roots();
        assert!(roots.contains("nestedpkg/outer"), "the outer package");
        assert!(roots.contains("nestedpkg/outer/inner"), "the nested one");

        assert_eq!(
            located("nestedpkg/outer/src/lib.rs", &roots).qualified_name("::"),
            "outer",
            "the outer package is unaffected by having a package inside it"
        );
        assert_eq!(
            located("nestedpkg/outer/src/a.rs", &roots).qualified_name("::"),
            "outer::a"
        );
        assert_eq!(
            located("nestedpkg/outer/inner/main.rs", &roots).qualified_name("::"),
            "inner",
            "a crate root in a nested package declares that package, not the outer one"
        );
        assert_eq!(
            located("nestedpkg/outer/inner/mod.rs", &roots).qualified_name("::"),
            "inner",
            "and it is the crate root module of `inner`, so it is the module `inner`"
        );
        assert_eq!(
            located("nestedpkg/outer/inner/b.rs", &roots).qualified_name("::"),
            "inner::b",
            "no `src` in this path, so the nearest package root above it is `inner`"
        );
    }

    #[test]
    fn a_package_root_under_a_source_root_is_the_directory_holding_the_source_root() {
        // `crates/foo/src/lib.rs` declares a package called `foo`, not one called `src`. The
        // directory holding a root module is the package; the source root inside it is not.
        let roots = roots_of(&["crates/foo/src/lib.rs", "crates/foo/src/a.rs"]);
        assert!(
            roots.contains("crates/foo"),
            "got {:?}",
            roots.iter().collect::<Vec<_>>()
        );
        assert!(
            !roots.contains("crates/foo/src"),
            "a source root is inside its package, not a package of its own: got {:?}",
            roots.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            located("crates/foo/src/a.rs", &roots).qualified_name("::"),
            "foo::a"
        );
    }

    #[test]
    fn a_directory_holding_a_root_module_deeper_than_a_source_root_is_not_a_package() {
        // `crates/foo/src/sub/main.rs` is the module `foo::sub::main`. It has the stem of a crate
        // root and sits in a directory, and it is neither — a crate root is at
        // `<package>/src/lib.rs` or `<package>/lib.rs`, not three levels down.
        let roots = roots_of(&[
            "crates/foo/src/lib.rs",
            "crates/foo/src/sub/main.rs",
            "crates/foo/src/sub/a.rs",
        ]);
        assert!(
            !roots.contains("crates/foo/src/sub"),
            "got {:?}",
            roots.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            located("crates/foo/src/sub/main.rs", &roots).qualified_name("::"),
            "foo::sub::main"
        );
    }

    // -----------------------------------------------------------------------
    // The package column is what it claims to be
    // -----------------------------------------------------------------------

    #[test]
    fn a_package_name_is_never_a_parent_directory_that_happens_to_match_another_row() {
        // The specific shape of the defect: a directory whose name collides with a real package
        // elsewhere in the tree. `tests/index/` is not a package, and `index` is the name of
        // `crates/index`, so a file in it must not claim the package `index` on the strength of
        // its own directory being called that — and must not declare one either.
        let roots = roots_of(&[
            "crates/index/src/lib.rs",
            "crates/index/src/index.rs",
            "tests/index/mod.rs",
            "tests/index/basic.rs",
        ]);
        assert!(
            !roots.contains("tests/index"),
            "`tests/index` holds a `mod.rs` and a test file, not a crate root"
        );
        for relative in ["tests/index/mod.rs", "tests/index/basic.rs"] {
            let packages: Vec<String> = modules_among(relative, &roots)
                .entities
                .iter()
                .filter(|entity| entity.kind() == EntityKind::Package)
                .map(|entity| entity.id.qualified_name().to_owned())
                .collect();
            assert!(
                packages.is_empty(),
                "{relative} declared a package called {packages:?} on the strength of its \
                 directory's name"
            );
        }
    }

    #[test]
    fn a_module_row_is_not_read_as_a_declaration_of_the_package_it_is_prefixed_with() {
        // The distinction the previous rule collapsed. A module row's name is prefixed with a
        // package so a qualified name can be spelled; only a root module file emits a `Package`.
        // So a file in an unrooted directory gets the fallback prefix *and* declares nothing,
        // and the two statements cannot be read as one.
        let roots = roots_of(&["crates/index/src/lib.rs", "tests/index/basic.rs"]);
        let located = located("tests/index/basic.rs", &roots);
        assert_eq!(
            located.package, "tests",
            "the fallback prefix is the file's own directory, which is `tests`"
        );
        let found = modules_among("tests/index/basic.rs", &roots);
        assert!(
            found
                .entities
                .iter()
                .all(|entity| entity.kind() != EntityKind::Package),
            "and it declares no package: {found:#?}"
        );
        assert!(
            found
                .entities
                .iter()
                .any(|entity| entity.kind() == EntityKind::Module),
            "it still gets a module, which is the point of the fallback"
        );
    }

    // -----------------------------------------------------------------------
    // What the repository fact does and does not cost
    // -----------------------------------------------------------------------

    #[test]
    fn a_path_under_a_source_root_is_answered_the_same_whether_or_not_the_roots_are_known() {
        // **The property the rule is designed around rather than a side effect of it.** A `src/`
        // layout reads nothing but the path, so every existing layout is byte-identical with and
        // without the repository's package roots — which is what keeps 43 of the five
        // repositories in the A/B unaffected by the change.
        let layout = rust_layout();
        let with = roots_of(&[
            "crates/regex-automata/src/lib.rs",
            "crates/regex-automata/src/util/look.rs",
            "crates/serde/src/lib.rs",
            "src/lib.rs",
            "tests/helper.rs",
            "crates/regex/src/inner/src/look.rs",
        ]);
        for relative in [
            "crates/regex-automata/src/lib.rs",
            "crates/regex-automata/src/util/look.rs",
            "crates/serde/src/lib.rs",
            "src/lib.rs",
            "crates/regex/src/inner/src/look.rs",
        ] {
            assert_eq!(
                locate(&path(relative), &layout, &with),
                locate(&path(relative), &layout, &no_roots()),
                "{relative} moved when the package roots were supplied"
            );
        }
    }

    #[test]
    fn a_file_is_only_asked_about_the_package_roots_when_its_path_names_no_source_root() {
        // What lets a refresh skip the read. Every `src/` file is answered from the path, so the
        // repository's paths are read only when something in the batch could need them.
        let layout = rust_layout();
        assert!(!super::needs_package_roots(
            [
                path("crates/foo/src/lib.rs"),
                path("crates/foo/src/a.rs"),
                path("crates/foo/src/a/mod.rs")
            ],
            layout
        ));
        assert!(super::needs_package_roots(
            [
                path("crates/foo/src/lib.rs"),
                path("crates/core/flags/defs.rs")
            ],
            layout
        ));
        assert!(
            super::needs_package_roots([path("tests/helper.rs")], layout),
            "a file with no source root at all is asked too, since the answer may change"
        );
    }

    #[test]
    fn no_package_roots_leaves_every_file_named_for_its_own_directory() {
        // What a caller that knows nothing about the repository gets, and it is the behaviour that
        // was there before the set existed — so `extract_with`, the single-file entry point, is
        // unchanged rather than degraded.
        let layout = rust_layout();
        assert_eq!(
            locate(&path("crates/core/flags/defs.rs"), &layout, &no_roots())
                .qualified_name("::"),
            "flags::defs"
        );
        assert_eq!(
            locate(&path("crates/matcher/tests/util.rs"), &layout, &no_roots())
                .qualified_name("::"),
            "tests::util",
            "and that is the name that collides with `tests/util.rs` in the same repository"
        );
    }

    #[test]
    fn package_roots_are_read_from_the_file_set_and_not_from_any_single_path() {
        // Both shapes, because the difference is the whole rule: `crates/core` holds a root module
        // directly, `crates/foo/src` holds one through a source root.
        let roots = roots_of(&["crates/core/main.rs", "crates/foo/src/lib.rs"]);
        assert!(roots.contains("crates/core"));
        assert!(roots.contains("crates/foo"));
        assert!(!roots.contains("crates"));
        assert!(
            roots.is_empty() || roots.len() == 2,
            "nothing else is a package root: {:?}",
            roots.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_module_of_a_package_with_no_source_root_still_declares_nothing() {
        // A `mod.rs` appearing in a package is not a package move: it names a module inside the
        // package that is already there. If it were treated as one, every refresh that touched a
        // `mod.rs` would re-extract the whole package.
        let roots = roots_of(&[
            "crates/core/main.rs",
            "crates/core/flags/defs.rs",
            "crates/core/flags/mod.rs",
        ]);
        assert!(roots.contains("crates/core"));
        assert!(!roots.contains("crates/core/flags"));
        let located = located("crates/core/flags/mod.rs", &roots);
        assert!(!located.is_package_root);
        assert_eq!(located.qualified_name("::"), "core::flags");
    }

    #[test]
    fn every_committed_fixture_file_is_source_the_extractor_accepts_without_complaint() {
        // A fixture that does not parse would make the module assertions above pass for the
        // wrong reason, because a degraded file still produces its module entity. Naming the
        // degradation is what stops that.
        let roots = fixture_roots();
        let mut degraded = Vec::new();
        for relative in NAMED_FIXTURES {
            let extracted = crate::extract::walker::extract_with_roots(
                registry::get(crate::model::Language::Rust).expect("rust spec"),
                path(relative),
                &fixture_source(relative),
                &roots,
            );
            if !extracted.is_clean() {
                degraded.push(format!("{relative}: {:?}", extracted.degradation));
            }
        }
        assert!(
            degraded.is_empty(),
            "a fixture that does not parse makes every assertion about it meaningless: {degraded:?}"
        );
    }
}
