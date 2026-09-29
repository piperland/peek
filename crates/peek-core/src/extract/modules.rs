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

    /// Assert that every fixture this test module names is actually on disk.
    ///
    /// A table of paths nothing checks is a list of intentions. This is what turns it into a list
    /// of files, and it fails as a *missing file* rather than as a puzzling assertion further
    /// down.
    #[test]
    fn every_module_fixture_named_by_these_tests_is_committed() {
        const NAMED: &[&str] = &[
            "README.md",
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
        ];
        let empty: Vec<&str> = NAMED
            .iter()
            .copied()
            .filter(|relative| fixture_source(relative).is_empty())
            .collect();
        assert!(empty.is_empty(), "fixtures that are committed but empty: {empty:?}");
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

    fn modules_for(p: &str) -> super::FileModules {
        let spec = registry::get(crate::model::Language::Rust).expect("rust spec");
        for_file(spec, &path(p), &file_id(p), span(), "fn a() {}")
    }

    fn module_names(p: &str) -> Vec<String> {
        modules_for(p)
            .entities
            .iter()
            .filter(|entity| entity.kind() == EntityKind::Module)
            .map(|entity| entity.id.qualified_name().to_owned())
            .collect()
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
        let layout = rust_layout();
        assert!(
            !locate(&path("crates/regex/src/a/main.rs"), &layout).is_package_root,
            "a main.rs below the source root is a module, not a package"
        );
        assert_eq!(
            locate(&path("crates/regex/src/a/main.rs"), &layout).qualified_name("::"),
            "regex::a::main"
        );
        assert!(
            locate(&path("crates/regex/src/lib.rs"), &layout).is_package_root,
            "a lib.rs directly in the source root is the package root"
        );
    }

    #[test]
    fn a_nested_source_directory_is_not_a_second_package_root() {
        // `src` is a directory name, not a reserved word. Taking the *first* one keeps every
        // file under it in one namespace; taking the last would give `inner` its own.
        let layout = rust_layout();
        let inner = locate(&path("crates/foo/src/inner/src/look.rs"), &layout);
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
        let located = locate(&path("crates/regex-automata/src/lib.rs"), &layout);
        assert_eq!(located.package, "regex_automata");
    }

    #[test]
    fn a_package_at_the_repository_root_is_named_after_its_source_root_and_says_so() {
        // The one approximation that produces a name which is simply wrong. It is pinned here so
        // that a future `Cargo.toml` reader can see exactly which case it has to fix, and so that
        // nobody discovers the behaviour from a qualified name in a report.
        let layout = rust_layout();
        let located = locate(&path("src/lib.rs"), &layout);
        assert_eq!(located.package, "src");
        assert!(located.is_package_root);
    }

    #[test]
    fn a_file_with_no_source_root_above_it_still_gets_a_module() {
        // Refusing to emit a module would leave every non-Cargo Rust file in the index without a
        // namespace, which is the same silent gap this whole change exists to close. The package
        // name is wrong here and there is no honest way to do better from a path alone.
        let layout = rust_layout();
        let located = locate(&path("tests/helper.rs"), &layout);
        assert_eq!(located.qualified_name("::"), "tests::helper");
    }

    #[test]
    fn a_mod_rs_directly_in_the_source_root_does_not_take_the_crate_root_s_name() {
        // `a/mod.rs` is the module `a` because there is a directory called `a`. A bare
        // `src/mod.rs` has no such directory, and naming it after the package would put two
        // different files in one namespace under one name.
        let layout = rust_layout();
        let located = locate(&path("crates/foo/src/mod.rs"), &layout);
        assert_eq!(located.qualified_name("::"), "foo::mod");
        assert!(!located.is_package_root);
        assert_ne!(
            located.qualified_name("::"),
            locate(&path("crates/foo/src/lib.rs"), &layout).qualified_name("::"),
        );
    }

    #[test]
    fn a_file_at_the_repository_root_falls_back_to_its_own_name() {
        // A file at the very top of a checkout has no directory above its source root and no
        // directory of its own. Something has to be chosen, and an empty package would make
        // every qualified name unmatchable.
        let layout = rust_layout();
        let located = locate(&path("toponly.rs"), &layout);
        assert_eq!(located.package, "toponly");
        assert_eq!(located.qualified_name("::"), "toponly::toponly");
        assert!(!located.is_package_root, "and it declares no package");
    }

    #[test]
    fn a_module_qualified_name_starts_with_the_package_even_when_there_are_no_segments() {
        let layout = rust_layout();
        let located = locate(&path("crates/regex/src/lib.rs"), &layout);
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
            before.as_deref().is_some_and(|value| value.starts_with("fnv1a64:")),
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
    fn fixture_modules(relative: &str) -> Vec<String> {
        let found = for_file(
            registry::get(crate::model::Language::Rust).expect("rust spec"),
            &path(relative),
            &file_id(relative),
            span(),
            &fixture_source(relative),
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
        let root = crate::extract::walker::extract_with(
            registry::get(crate::model::Language::Rust).expect("rust spec"),
            path("several/src/lib.rs"),
            &fixture_source("several/src/lib.rs"),
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
        let bare = locate(&path("cross/alpha/src/gateway.rs"), &layout);
        let in_repository = locate(
            &path("crates/peek-core/tests/fixtures/modules/cross/alpha/src/gateway.rs"),
            layout,
        );
        assert_eq!(bare, in_repository);
        assert_eq!(in_repository.qualified_name("::"), "alpha::gateway");
    }

    #[test]
    fn every_committed_fixture_file_is_source_the_extractor_accepts_without_complaint() {
        // A fixture that does not parse would make the module assertions above pass for the
        // wrong reason, because a degraded file still produces its module entity. Naming the
        // degradation is what stops that.
        const NAMED: &[&str] = &[
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
        ];
        let mut degraded = Vec::new();
        for relative in NAMED {
            let extracted = crate::extract::walker::extract_with(
                registry::get(crate::model::Language::Rust).expect("rust spec"),
                path(relative),
                &fixture_source(relative),
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
