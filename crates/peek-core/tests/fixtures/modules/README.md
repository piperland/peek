# Module fixture trees

Committed trees for the module and package rules in `peek_core::extract`. Every file here is read
by a test: if a test needs a file that is not committed, the test fails rather than creating it,
and if a file here is not asserted on by a test it is a file nobody asked for.

The paths in the tests are **relative to this directory**, which is what a repository-relative
path looks like after the rest of the tree is stripped off. For a tree under a `src` the rule reads
only the path, so a fixture laid out as `several/src/lib.rs` produces the same package name as
`crates/peek-core/tests/fixtures/modules/several/src/lib.rs` would, and one test asserts both. For
a tree with no `src` the rule also reads the repository's package roots, which are built from
**these paths and no others** — so a fixture's answer depends on which other fixtures are in this
directory, and adding a file to `nosrc/` can change what a file already there is called. That is
the rule working rather than a hazard: it is exactly what happens when a `main.rs` appears in a
real crate.

| Tree | Shape | What it proves |
|---|---|---|
| `nested/` | `mod` three levels deep, every level in its own `mod.rs` | a `mod.rs` file is the module its *directory* names, and a `mod x;` is that module's child |
| `several/` | one crate, three module files, one inline module, one re-export | a file's module name is its path from the crate root, and an inline `mod` keeps the name it always had |
| `cross/` | two packages, one `use` from one into the other | a cross-crate import is a path the module table can be asked about |
| `reexport/` | `pub use`, a renamed re-export, and a glob re-export | a re-export is still a binding, and a glob binds a star rather than a name |
| `nosrc/` | one crate with **no `src/` at all**, four directories deep, with a `mod.rs` at two levels | a file's package is the nearest ancestor that holds a crate root, and one crate is one package however deep it is |
| `nestedpkg/` | a package with a `src/` **and** a nested package with a `main.rs` inside it | nearest package root wins, so the two namespaces stay separate and a cross-crate path stays spellable |

`nosrc/` and `nestedpkg/` are the trees that cannot be read off a path. `nosrc/core/flags/defs.rs`
and `nestedpkg/outer/inner/b.rs` have no `src` component, so nothing in the path says which
ancestor directory their crate begins at — `nosrc/core/flags/defs.rs` could be a module of a crate
rooted at `crates/core` or a file in a package of its own called `flags`, and the paths are the
same either way. The tests therefore build the extractor a [`PackageRoots`] from **this whole
tree**, exactly as a full build of a real repository would, and that is also what makes these
fixtures a test of the repository-level half of the rule rather than only of the path-level half.

[`PackageRoots`]: ../../../../src/extract/modules.rs

`modules/` is deliberately **not** a git repository and has no `.gitignore`, so nothing in it is
excluded and every file is a file the tests actually read. `git clean -fdx` in the verification
sandbox is then safe for this tree by construction rather than by remembering to pass a flag.
