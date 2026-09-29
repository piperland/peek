# Module fixture trees

Committed trees for the module and package rules in `peek_core::extract`. Every file here is read
by a test: if a test needs a file that is not committed, the test fails rather than creating it,
and if a file here is not asserted on by a test it is a file nobody asked for.

The paths in the tests are **relative to this directory**, which is what a repository-relative
path looks like after the rest of the tree is stripped off. The rule under test only looks at
where a `src` directory sits and at the directory above it, so a fixture laid out as
`several/src/lib.rs` produces the same package name as
`crates/peek-core/tests/fixtures/modules/several/src/lib.rs` would. One test asserts both, so the
fixture cannot quietly stop proving what it was written to prove.

| Tree | Shape | What it proves |
|---|---|---|
| `nested/` | `mod` three levels deep, every level in its own `mod.rs` | a `mod.rs` file is the module its *directory* names, and a `mod x;` is that module's child |
| `several/` | one crate, three module files, one inline module, one re-export | a file's module name is its path from the crate root, and an inline `mod` keeps the name it always had |
| `cross/` | two packages, one `use` from one into the other | a cross-crate import is a path the module table can be asked about |
| `reexport/` | `pub use`, a renamed re-export, and a glob re-export | a re-export is still a binding, and a glob binds a star rather than a name |

`modules/` is deliberately **not** a git repository and has no `.gitignore`, so nothing in it is
excluded and every file is a file the tests actually read. `git clean -fdx` in the verification
sandbox is then safe for this tree by construction rather than by remembering to pass a flag.
